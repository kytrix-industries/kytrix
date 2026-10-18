//! The Nasdaq symbol directory the venue's listings are read from.
//!
//! Two pipe-separated files state which securities exist, what they are called and
//! which market each of them trades on, and both of them are downloaded and parsed
//! whole: a body that does not start with the header this build knows, or a row
//! whose field count is not the one its header states, is a file this build cannot
//! read and fails the entire load rather than publishing a partial directory.
//!
//! The directory is not served by the venue, so the download dials the market participant's
//! own host rather than a route's origin — through the execution's HTTP client, so
//! it takes the venue's proxy line like every other request the execution makes.
//! The address is the one seam a test replaces.

use std::{str, time::Duration};

use bytes::{Bytes, BytesMut};
use futures::StreamExt as _;
use market_participants_model::identifier::{Mic, VenueId};
use smol_str::{SmolStr, format_smolstr};
use url::Url;

use crate::{
    adapter::ExecutionContext, catalog::resolved::ResolvedListingRef, error::MarketParticipantError,
};

/// Where the Nasdaq symbol directory is published.
///
/// The files are served over HTTPS from the market participant's own site. The venue also
/// mirrors the same content over FTP; this build does not speak that protocol and
/// reaches the HTTPS copy instead, so the directory costs no dependency beyond the
/// HTTP client every adapter already has.
pub(crate) const NASDAQ_SYMBOL_DIRECTORY: &str = "https://www.nasdaqtrader.com/dynamic/SymDir/";
pub(crate) const NASDAQ_LISTED_FILE: &str = "nasdaqlisted.txt";
pub(crate) const OTHER_LISTED_FILE: &str = "otherlisted.txt";

/// The largest symbol directory this build reads.
///
/// Both files together are well under this today (measured 2026-10-11: 349 479 +
/// 543 175 bytes, about 0.85 MiB); the bound exists so a wrong answer — an error
/// page that keeps streaming, a redirect to something that is not the directory —
/// cannot make the load allocate without end.
pub(crate) const SYMBOL_DIRECTORY_MAX_BYTES: usize = 16 * 1024 * 1024;

/// How long one symbol directory download may take, headers and body together.
pub(crate) const SYMBOL_DIRECTORY_TIMEOUT: Duration = Duration::from_secs(30);

/// The header `nasdaqlisted.txt` starts with.
const NASDAQ_LISTED_HEADER: &str = "Symbol|Security Name|Market Category|Test Issue|Financial Status|Round Lot Size|ETF|NextShares";

/// The header `otherlisted.txt` starts with.
const OTHER_LISTED_HEADER: &str =
    "ACT Symbol|Security Name|Exchange|CQS Symbol|ETF|Round Lot Size|Test Issue|NASDAQ Symbol";

/// The market every row of `nasdaqlisted.txt` trades on.
const NASDAQ_LISTED_MIC: &str = "XNAS";

/// The market suffix the venue addresses its US listings by.
///
/// The directory publishes bare codes and the venue addresses securities as
/// `{code}.US`, so this is the one half of a native symbol the directory does not
/// state. Both the catalog construction and the wire read it, which is why it lives
/// here rather than beside either of them.
pub(super) const MARKET_SUFFIX: &str = "US";

/// The field the generation timestamp line starts with.
const FILE_CREATION_TIME: &str = "File Creation Time";

/// The longest code the directory's own field definition admits.
///
/// The field is a root of one to five characters plus up to three suffix
/// characters, and the definition caps a whole code at fourteen; that cap is the
/// bound this build reads. A longer code is a file this build does not understand
/// rather than a security it publishes.
const MAX_CODE_LENGTH: usize = 14;

/// One row of one symbol directory file, before canonical construction.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct SymbolRow {
    /// The code the venue is addressed by, without the market suffix.
    pub code: SmolStr,
    /// The company or security name the file publishes.
    pub name: SmolStr,
    /// The market the security is actually listed on.
    pub mic: Mic,
}

/// The columns of one symbol directory file this build reads.
#[derive(Clone, Copy)]
struct DirectoryLayout {
    /// The exact header line the file starts with.
    header: &'static str,
    /// The number of fields that header states.
    columns: usize,
    /// The column carrying the code the venue is addressed by.
    code: usize,
    /// The column carrying the company or security name.
    name: usize,
    /// The column carrying the test-issue flag.
    test_issue: usize,
    /// The column carrying the ETF flag.
    etf: usize,
    /// How the file states the market a row trades on.
    market: MarketColumn,
}

/// How one symbol file states the market its rows trade on.
#[derive(Clone, Copy)]
enum MarketColumn {
    /// Every row of the file is this market's, whatever else the row says.
    Whole(&'static str),
    /// The row's own market-participant code names the market.
    PerRow(usize),
}

/// The columns of `nasdaqlisted.txt`.
const NASDAQ_LISTED_LAYOUT: DirectoryLayout = DirectoryLayout {
    header: NASDAQ_LISTED_HEADER,
    columns: 8,
    code: 0,
    name: 1,
    test_issue: 3,
    etf: 6,
    market: MarketColumn::Whole(NASDAQ_LISTED_MIC),
};

/// The columns of `otherlisted.txt`.
const OTHER_LISTED_LAYOUT: DirectoryLayout = DirectoryLayout {
    header: OTHER_LISTED_HEADER,
    columns: 8,
    code: 0,
    name: 1,
    test_issue: 6,
    etf: 4,
    market: MarketColumn::PerRow(2),
};

/// The market of one `otherlisted.txt` market-participant code.
///
/// The table is the one the design states: the five venues Nasdaq's own field
/// definitions list, plus `F`, which those definitions do not list but whose
/// CTA/UTP market-participant code is the Texas Stock Exchange. `M` is deliberately absent:
/// the sources that name it disagree about which market it is, and a MIC invented
/// out of a conflict would be a well-formed identity for the wrong market. A code
/// this table does not name is dropped with a warning rather than guessed at.
const MARKET_PARTICIPANT_MICS: [(&str, &str); 6] = [
    ("N", "XNYS"),
    ("A", "XASE"),
    ("P", "ARCX"),
    ("Z", "BATS"),
    ("V", "IEXG"),
    ("F", "TXSE"),
];

/// Downloads and parses both Nasdaq symbol files.
pub(crate) async fn fetch_symbol_rows(
    context: &ExecutionContext,
) -> Result<Vec<SymbolRow>, MarketParticipantError> {
    let mut rows = Vec::with_capacity(12_000);
    rows.extend(download_and_parse(context, NASDAQ_LISTED_FILE).await?);
    rows.extend(download_and_parse(context, OTHER_LISTED_FILE).await?);
    Ok(rows)
}

/// The symbol one listing is addressed by.
///
/// The venue addresses its securities as `{code}.{MARKET_SUFFIX}`, and the symbol a
/// request sends is the one the catalog published rather than one built from the key:
/// the directory is where the code came from, and a symbol rebuilt here — or by any
/// other reader — would be a second spelling of the same listing. A listing this
/// adapter cannot address is a request it cannot encode rather than one to guess at.
pub(crate) fn market_symbol(
    listing: &ResolvedListingRef,
) -> Result<SmolStr, MarketParticipantError> {
    let market_participants_model::listing::identifiers::VenueListingIdentifiers::Equity(_) =
        &listing.identifiers
    else {
        return Err(unsupported_listing(listing));
    };
    listing
        .native_symbol
        .as_ref()
        .map(|symbol| SmolStr::new(symbol.as_str()))
        .ok_or_else(|| unsupported_listing(listing))
}

/// The failure of a listing this adapter cannot address.
fn unsupported_listing(listing: &ResolvedListingRef) -> MarketParticipantError {
    MarketParticipantError::ProtocolError {
        operation: SmolStr::new_static("encode Longbridge request"),
        reason: format_smolstr!(
            "the listing {} is not a US equity of this venue",
            listing.key.listing_id
        ),
        raw_data: None,
    }
}

/// Downloads one symbol file and parses it.
///
/// A failure of either half is the failure of one operation — reading this venue's
/// symbol directory — so both a download that could not be read and a file that
/// could not be parsed are reported the same way, with the address or the concrete
/// cause as the diagnostic detail.
async fn download_and_parse(
    context: &ExecutionContext,
    file: &'static str,
) -> Result<Vec<SymbolRow>, MarketParticipantError> {
    let url = symbol_directory_root().join(file).map_err(|error| {
        symbol_directory_failed(format_smolstr!(
            "{file} is not a path under the directory: {error}"
        ))
    })?;
    let body = tokio::time::timeout(
        SYMBOL_DIRECTORY_TIMEOUT,
        read_symbol_file(context, url.clone()),
    )
    .await
    .map_err(|_| {
        symbol_directory_failed(format_smolstr!(
            "{url} did not answer within {}s",
            SYMBOL_DIRECTORY_TIMEOUT.as_secs()
        ))
    })??;
    parse(file, &body)
}

/// Reads one symbol file body, bounded by [`SYMBOL_DIRECTORY_MAX_BYTES`].
///
/// The body is streamed rather than buffered whole, so the bound is enforced while
/// it arrives: a `content-length` is a claim, not a guarantee.
async fn read_symbol_file(
    context: &ExecutionContext,
    url: Url,
) -> Result<Bytes, MarketParticipantError> {
    let response = context
        .http_client
        .get(url.clone())
        .send()
        .await
        .map_err(|error| {
            symbol_directory_failed(format_smolstr!("{url} could not be read: {error}"))
        })?;
    let status = response.status();
    if !status.is_success() {
        return Err(symbol_directory_failed(format_smolstr!(
            "{url} answered {status}"
        )));
    }
    let mut body = BytesMut::with_capacity(2 * 1024 * 1024);
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|error| {
            symbol_directory_failed(format_smolstr!(
                "{url} stopped before its body ended: {error}"
            ))
        })?;
        if body.len() + chunk.len() > SYMBOL_DIRECTORY_MAX_BYTES {
            return Err(symbol_directory_failed(format_smolstr!(
                "{url} is larger than the {SYMBOL_DIRECTORY_MAX_BYTES} bytes this build reads"
            )));
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body.freeze())
}

/// Parses one symbol directory file into rows.
///
/// The file is pipe-separated with a header line and a trailing generation
/// timestamp, and both are part of the format rather than data: a body that does
/// not start with the header this build knows, or a row with a different field
/// count, is a file this build cannot read and fails the whole load instead of
/// publishing a partial directory.
///
/// A row the directory states but this build will not publish — a test issue, an
/// ETF, a code it cannot address, a market it cannot name — is a product decision
/// rather than a protocol failure, so it is dropped with a warning and the load
/// continues.
pub(crate) fn parse(file: &str, body: &[u8]) -> Result<Vec<SymbolRow>, MarketParticipantError> {
    let layout = layout_of(file)?;
    let text = str::from_utf8(body)
        .map_err(|error| symbol_directory_failed(format_smolstr!("{file} is not text: {error}")))?;
    let mut lines = text.split('\n').map(|line| line.trim_end_matches('\r'));
    let header = lines.next().unwrap_or_default();
    if header != layout.header {
        return Err(symbol_directory_failed(format_smolstr!(
            "{file} starts with {header:?}, not the header this build reads"
        )));
    }
    let mut rows = Vec::with_capacity(6_000);
    for (offset, line) in lines.enumerate() {
        if line.is_empty() {
            continue;
        }
        let mut fields = Vec::with_capacity(layout.columns);
        fields.extend(line.split('|'));
        // The generation timestamp is recognised before the field count is judged:
        // `otherlisted.txt` states its timestamp with seven fields where its rows
        // carry eight, so a parser that counted first would refuse the whole file as
        // a protocol error. Nothing after the timestamp is a row either.
        if fields[0].starts_with(FILE_CREATION_TIME) {
            break;
        }
        let row = offset + 2;
        if fields.len() != layout.columns {
            return Err(symbol_directory_failed(format_smolstr!(
                "{file} line {row} carries {} fields, not {}",
                fields.len(),
                layout.columns
            )));
        }
        if fields[layout.test_issue] == "Y" || fields[layout.etf] == "Y" {
            continue;
        }
        let code = fields[layout.code];
        let name = fields[layout.name].trim();
        if code.is_empty() || name.is_empty() {
            log::warn!("the Longbridge symbol directory {file} line {row} states no code or name");
            continue;
        }
        if !addressable_code(code) {
            log::warn!(
                "the Longbridge symbol directory {file} line {row} states a code this build cannot address: {code:?}"
            );
            continue;
        }
        let Some(mic) = market_mic(layout.market, &fields) else {
            log::warn!(
                "the Longbridge symbol directory {file} line {row} names a market this build cannot place"
            );
            continue;
        };
        rows.push(SymbolRow {
            code: SmolStr::new(code),
            name: SmolStr::new(name),
            mic,
        });
    }
    rows.sort_unstable_by(|left, right| left.code.cmp(&right.code));
    rows.dedup_by(|left, right| left.code == right.code);
    Ok(rows)
}

/// Reports whether one directory code is a symbol this build can publish.
///
/// A code the directory states is ASCII — the field is a symbol, and the venue it is
/// published by addresses securities with ASCII tickers — so a code outside that
/// alphabet is a file this build is not reading rather than a security it cannot name.
/// The judgement is a data-quality rule: the row is dropped with a warning and the load
/// continues ([`SymbolRow`]), because the file is unsigned remote content fetched on
/// every load and one unreadable row must not take the venue's whole catalog down.
///
/// What it admits is exactly what the catalog construction can build from the row, one
/// step earlier: letters and digits, plus the class designators the directory uses for the
/// symbols that are not one bare root — the `.` of `BRK.B`, and the `-`, `^` and `$` the
/// issuer scrub treats as designators — and at least one alphanumeric of the root those
/// designators delimit. The rule is the whole code and not a prefix of it: a character
/// outside that alphabet anywhere is a row whose identifiers the constructors behind this
/// parse would refuse, so it is dropped here rather than handed on. A code with no such
/// root names no issuer — the empty root of `...` and the designator-only code are the
/// same row to this judgement — and the root rather than the prefix is what the
/// alphanumeric is counted in, so the leading designator of a preferred share (`$XYZ`) or
/// an index (`^IXIC`) is a code this build addresses rather than one it drops. The length
/// bound is the directory's own field definition ([`MAX_CODE_LENGTH`]).
fn addressable_code(code: &str) -> bool {
    if code.is_empty() || code.len() > MAX_CODE_LENGTH {
        return false;
    }
    let root = code.trim_start_matches(['.', '-', '^', '$']);
    let mut root_chars = 0_usize;
    for character in code.chars() {
        if character.is_ascii_alphanumeric() {
            root_chars += 1;
        } else if !matches!(character, '.' | '-' | '^' | '$') {
            return false;
        }
    }
    root_chars > 0
        && root
            .chars()
            .any(|character| character.is_ascii_alphanumeric())
}

/// The layout of one symbol file, by the name it is published under.
fn layout_of(file: &str) -> Result<DirectoryLayout, MarketParticipantError> {
    match file {
        NASDAQ_LISTED_FILE => Ok(NASDAQ_LISTED_LAYOUT),
        OTHER_LISTED_FILE => Ok(OTHER_LISTED_LAYOUT),
        _ => Err(symbol_directory_failed(format_smolstr!(
            "{file} is not a symbol directory this build reads"
        ))),
    }
}

/// The market one row trades on, when the file states one this build knows.
fn market_mic(market: MarketColumn, fields: &[&str]) -> Option<Mic> {
    let code = match market {
        MarketColumn::Whole(mic) => mic,
        MarketColumn::PerRow(column) => MARKET_PARTICIPANT_MICS
            .iter()
            .find(|(market_participant, _)| *market_participant == fields[column])
            .map(|(_, mic)| *mic)?,
    };
    // The table above is a table of canonical MICs, so a code it names is one the
    // model accepts: what is refused here is the venue's answer, never this table.
    Some(
        Mic::new(code)
            .expect("the market-participant table names canonical market identifier codes"),
    )
}

/// The address symbol directories are read from.
///
/// The default is the market participant's own site. A build that carries `test-support`
/// replaces it for the length of one test, so a test can serve its own directory
/// instead of reaching the internet.
#[cfg(not(feature = "test-support"))]
fn symbol_directory_root() -> Url {
    Url::parse(NASDAQ_SYMBOL_DIRECTORY).expect("the published directory address is a URL")
}

/// The address symbol directories are read from, replaceable by a test.
#[cfg(feature = "test-support")]
fn symbol_directory_root() -> Url {
    crate::test_support::symbol_directory_root(NASDAQ_SYMBOL_DIRECTORY)
}

/// Builds one symbol-directory failure that names the venue it is about.
fn symbol_directory_failed(reason: impl Into<SmolStr>) -> MarketParticipantError {
    MarketParticipantError::SymbolDirectoryFetchFailed {
        venue: VenueId::Longbridge,
        reason: reason.into(),
    }
}

#[cfg(test)]
mod tests {
    use market_participants_model::{
        identifier::{VenueId, VenueScopeId},
        listing::route::VenueRoute,
        routing::CapabilityId,
    };
    use url::Url;

    use super::{
        NASDAQ_LISTED_FILE, NASDAQ_LISTED_HEADER, NASDAQ_SYMBOL_DIRECTORY, OTHER_LISTED_FILE,
        OTHER_LISTED_HEADER, SYMBOL_DIRECTORY_MAX_BYTES, fetch_symbol_rows, parse,
        symbol_directory_root,
    };
    use crate::{
        error::MarketParticipantError,
        routing::LONGBRIDGE_US_SCOPE,
        test_support::{
            self, DirectoryAnswer, NASDAQ_LISTED_SAMPLE, OTHER_LISTED_SAMPLE,
            SymbolDirectoryFixture,
        },
    };

    /// The route these tests read the directory for.
    fn route() -> VenueRoute {
        VenueRoute::new(
            VenueId::Longbridge,
            VenueScopeId::new(LONGBRIDGE_US_SCOPE).expect("the US scope is canonical"),
        )
    }

    /// One execution whose HTTP client dials loopback over plaintext.
    fn context() -> crate::adapter::ExecutionContext {
        test_support::loopback_execution(route(), CapabilityId::MarketCatalog)
    }

    /// One file of `otherlisted.txt` whose rows state what a test wants to read.
    fn other_listed(rows: &[&str]) -> String {
        format!("{OTHER_LISTED_HEADER}\r\n{}\r\n", rows.join("\r\n"))
    }

    /// One file of `nasdaqlisted.txt` whose rows state what a test wants to read.
    fn nasdaq_listed(rows: &[&str]) -> String {
        format!("{NASDAQ_LISTED_HEADER}\r\n{}\r\n", rows.join("\r\n"))
    }

    /// The codes one parse published, in order.
    fn codes(file: &str, body: &str) -> Vec<String> {
        parse(file, body.as_bytes())
            .expect("the fixture file parses")
            .into_iter()
            .map(|row| row.code.to_string())
            .collect()
    }

    /// The design's own samples parse, and the ETF row of each of them does not
    /// become a listing.
    #[test]
    fn the_published_samples_parse_into_their_listings() {
        let nasdaq = parse(NASDAQ_LISTED_FILE, NASDAQ_LISTED_SAMPLE.as_bytes())
            .expect("the nasdaqlisted sample parses");
        assert_eq!(nasdaq.len(), 1, "the ETF row is not a listing");
        assert_eq!(nasdaq[0].code, "AACG");
        assert_eq!(nasdaq[0].mic.as_str(), "XNAS");
        assert_eq!(
            nasdaq[0].name,
            "ATA Creativity Global - American Depositary Shares, each representing two common shares"
        );

        let other = parse(OTHER_LISTED_FILE, OTHER_LISTED_SAMPLE.as_bytes())
            .expect("the otherlisted sample parses");
        assert_eq!(
            other
                .iter()
                .map(|row| row.code.as_str())
                .collect::<Vec<_>>(),
            ["A", "AA"],
            "the ETF row is dropped and the rest keep the directory's order"
        );
        assert_eq!(other[0].mic.as_str(), "XNYS");
        assert_eq!(other[1].mic.as_str(), "XNYS");
    }

    /// The generation timestamp ends the file, and it is judged before the field
    /// count: `otherlisted.txt` states it with seven fields where its rows carry
    /// eight, so a parser that counted first would refuse the whole file.
    #[test]
    fn the_generation_timestamp_ends_both_files_before_their_field_counts() {
        let body = format!(
            "{OTHER_LISTED_HEADER}\r\n\
             A|Agilent Technologies, Inc. Common Stock|N|A|N|100|N|A\r\n\
             File Creation Time: 1009202621:31||||||\r\n\
             ZZZ|A Row After The Timestamp|N|ZZZ|N|100|N|ZZZ\r\n"
        );

        let rows = parse(OTHER_LISTED_FILE, body.as_bytes())
            .expect("a seven-field timestamp is not a protocol error");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].code, "A");

        let body = format!(
            "{NASDAQ_LISTED_HEADER}\r\n\
             AACG|ATA Creativity Global|S|N|D|100|N|N\r\n\
             File Creation Time: 1009202621:31|||||||\r\n\
             ZZZ|A Row After The Timestamp|S|N|D|100|N|N\r\n"
        );
        let rows = parse(NASDAQ_LISTED_FILE, body.as_bytes())
            .expect("an eight-field timestamp ends the file");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].code, "AACG");
    }

    /// A test issue and an ETF are the market participant's own statements about a row that is
    /// not a listed security, on both files.
    #[test]
    fn a_test_issue_and_an_etf_are_not_listings() {
        let nasdaq = nasdaq_listed(&[
            "ZTEST|A Test Issue|S|Y|D|100|N|N",
            "ZETF|A Fund|S|N|D|100|Y|N",
            "AREAL|A Real Security|S|N|D|100|N|N",
        ]);
        assert_eq!(codes(NASDAQ_LISTED_FILE, &nasdaq), ["AREAL"]);

        let other = other_listed(&[
            "MTEST|NYSE Texas, Inc. TEST Common Stock|M|MTEST|N|100|Y|MTEST",
            "AAA|Alternative Access First Priority CLO Bond ETF|P|AAA|Y|100|N|AAA",
            "AREAL|A Real Security|N|AREAL|N|100|N|AREAL",
        ]);
        assert_eq!(codes(OTHER_LISTED_FILE, &other), ["AREAL"]);
    }

    /// A name the file padded is published as the name it states, and the row that
    /// carries the padding is the design's own sample line.
    #[test]
    fn a_padded_name_loses_its_trailing_space() {
        let rows = parse(OTHER_LISTED_FILE, OTHER_LISTED_SAMPLE.as_bytes())
            .expect("the otherlisted sample parses");
        let alcoa = rows
            .iter()
            .find(|row| row.code == "AA")
            .expect("the sample lists Alcoa");

        assert_eq!(alcoa.name, "Alcoa Corporation Common Stock");
    }

    /// Every market-participant code the directory publishes maps onto the market it is, and
    /// one it does not name is dropped rather than guessed at.
    #[test]
    fn every_market_participant_code_maps_onto_its_market() {
        let other = other_listed(&[
            "ANYSE|A New York Listing|N|ANYSE|N|100|N|ANYSE",
            "AMKT|An NYSE MKT Listing|A|AMKT|N|100|N|AMKT",
            "AARCA|An Arca Listing|P|AARCA|N|100|N|AARCA",
            "ABATS|A Bats Listing|Z|ABATS|N|100|N|ABATS",
            "AIEX|An IEX Listing|V|AIEX|N|100|N|AIEX",
            "DDS|Dillard's, Inc. Common Stock|F|DDS|N|100|N|DDS",
            "MTEST|NYSE Texas, Inc. TEST Common Stock|M|MTEST|N|100|N|MTEST",
        ]);

        let rows = parse(OTHER_LISTED_FILE, other.as_bytes()).expect("the fixture file parses");
        let markets = rows
            .iter()
            .map(|row| (row.code.as_str(), row.mic.as_str()))
            .collect::<Vec<_>>();
        assert_eq!(
            markets,
            [
                ("AARCA", "ARCX"),
                ("ABATS", "BATS"),
                ("AIEX", "IEXG"),
                ("AMKT", "XASE"),
                ("ANYSE", "XNYS"),
                ("DDS", "TXSE"),
            ],
            "the Texas Stock Exchange code is the one the field definitions omit, and M is not placed"
        );
    }

    /// A code the field never states, or one this build cannot address, is a data
    /// quality decision rather than a protocol failure: the row is dropped and the
    /// file still loads.
    #[test]
    fn a_row_this_build_cannot_address_is_not_published() {
        let other = other_listed(&[
            "AX|An Unknown Exchange|X|AX|N|100|N|AX",
            "AEMPTY||N|AEMPTY|N|100|N|AEMPTY",
            "||N|NOMARKET|N|100|N|NOMARKET",
            "ATOOLONGCODE1234|A Code Past The Field|N|ATOOLONG|N|100|N|ATOOLONG",
            "ASP ACE|A Code With A Space|N|ASPACE|N|100|N|ASPACE",
            "AREAL|A Real Security|N|AREAL|N|100|N|AREAL",
        ]);

        assert_eq!(codes(OTHER_LISTED_FILE, &other), ["AREAL"]);

        let nasdaq = nasdaq_listed(&["", "AREAL|A Real Security|S|N|D|100|N|N"]);
        assert_eq!(
            codes(NASDAQ_LISTED_FILE, &nasdaq),
            ["AREAL"],
            "an empty line is skipped rather than read as a row"
        );
    }

    /// A code outside the alphabet the directory's own symbols use is dropped here, so
    /// that the catalog construction behind this parse is never handed a row it must
    /// refuse: a `NativeSymbol` refuses a control character, a display code refuses one
    /// too, and a code whose root holds no alphanumeric names no issuer at all. The
    /// directory is unsigned content fetched on every load, so one such row is one row
    /// dropped and not the venue's whole catalog.
    ///
    /// The first two codes of each file are the ones a review executed against the
    /// parser: `A.B\0C` and `A$B\u{7}` carry a control character **after** a designator,
    /// which a rule that stopped at the first designator published — and which the
    /// catalog construction then refused, taking the whole venue's catalog down with it.
    /// The rule is the whole code. What stays includes the leading class designator the
    /// directory really publishes: `$XYZ` and `^IXIC` are rows the issuer scrub names a
    /// root for, so they are addressed rather than dropped.
    #[test]
    fn a_code_outside_the_directories_own_alphabet_is_dropped() {
        let nasdaq = nasdaq_listed(&[
            "A.B\u{0}C|A Control Character After A Designator|S|N|D|100|N|N",
            "A$B\u{7}|A Control Character After A Mark|S|N|D|100|N|N",
            "A\u{0}B|A Control Character In The Root|S|N|D|100|N|N",
            "ÄÖÜ|A Code Outside ASCII|S|N|D|100|N|N",
            "...|A Code Of Designators Alone|S|N|D|100|N|N",
            "A B|A Code With A Space|S|N|D|100|N|N",
            "$XYZ|A Preferred Share|S|N|D|100|N|N",
            "^IXIC|An Index|S|N|D|100|N|N",
            "BRK.B|Berkshire Hathaway Class B|S|N|D|100|N|N",
            "AA|Alcoa|S|N|D|100|N|N",
        ]);

        assert_eq!(
            codes(NASDAQ_LISTED_FILE, &nasdaq),
            ["$XYZ", "AA", "BRK.B", "^IXIC"],
            "only the codes the directory's own alphabet admits are published, in the \
             directory's own order"
        );

        // The rule is the alphabet, not the length: the longest code the field admits
        // passes, and the one past it does not.
        let other = other_listed(&[
            "ABCDE12345678|A Code Of Thirteen Characters|N|ACODE13|N|100|N|ACODE13",
            "ABCDE123456789|A Code Of Fourteen Characters|N|ACODE14|N|100|N|ACODE14",
            "ABCDE1234567890|A Code Of Fifteen Characters|N|ACODE15|N|100|N|ACODE15",
            "AREAL|A Real Security|N|AREAL|N|100|N|AREAL",
        ]);
        assert_eq!(
            codes(OTHER_LISTED_FILE, &other),
            ["ABCDE12345678", "ABCDE123456789", "AREAL"],
            "the field's own fourteen-character cap is the bound, and one past it is not a row"
        );
    }

    /// A file whose header this build does not know is refused whole, and so is a
    /// row whose field count is not the one the header states.
    #[test]
    fn a_file_this_build_cannot_read_is_refused_whole() {
        let wrong_header = "Symbol|Security Name|Market Category|Test Issue|Financial Status|Round Lot Size\r\n\
             AACG|ATA Creativity Global|S|N|D|100\r\n";
        assert!(matches!(
            parse(NASDAQ_LISTED_FILE, wrong_header.as_bytes()),
            Err(MarketParticipantError::SymbolDirectoryFetchFailed { .. })
        ));

        let short_row = other_listed(&["A|Agilent Technologies, Inc. Common Stock|N|A|N|100|N"]);
        assert!(matches!(
            parse(OTHER_LISTED_FILE, short_row.as_bytes()),
            Err(MarketParticipantError::SymbolDirectoryFetchFailed { .. })
        ));

        let long_row = other_listed(&["A|Agilent|N|A|N|100|N|A|extra"]);
        assert!(matches!(
            parse(OTHER_LISTED_FILE, long_row.as_bytes()),
            Err(MarketParticipantError::SymbolDirectoryFetchFailed { .. })
        ));

        let empty = parse(NASDAQ_LISTED_FILE, b"");
        assert!(
            matches!(
                empty,
                Err(MarketParticipantError::SymbolDirectoryFetchFailed { .. })
            ),
            "a body with no header at all is not a directory"
        );

        let unknown_file = parse("somewhereelse.txt", NASDAQ_LISTED_SAMPLE.as_bytes());
        assert!(matches!(
            unknown_file,
            Err(MarketParticipantError::SymbolDirectoryFetchFailed { .. })
        ));
    }

    /// The directory is published with Windows line endings, and read with either.
    #[test]
    fn both_line_endings_parse_the_same_file() {
        let windows = OTHER_LISTED_SAMPLE;
        let unix = windows.replace("\r\n", "\n");

        assert_eq!(
            parse(OTHER_LISTED_FILE, unix.as_bytes()).expect("the unix sample parses"),
            parse(OTHER_LISTED_FILE, windows.as_bytes()).expect("the windows sample parses")
        );
    }

    /// The rows are sorted and de-duplicated by code, so one listing is one row.
    #[test]
    fn the_rows_are_sorted_and_deduplicated_by_code() {
        let other = other_listed(&[
            "ZZZ|The Last One|N|ZZZ|N|100|N|ZZZ",
            "AAA|The First One|N|AAA|N|100|N|AAA",
            "AAA|The First One Again|N|AAA|N|100|N|AAA",
        ]);

        assert_eq!(codes(OTHER_LISTED_FILE, &other), ["AAA", "ZZZ"]);
    }

    /// The address a read dials is the published one, a fixture replaces it while it
    /// is installed, and its guard restores what was in place before it.
    #[tokio::test]
    async fn the_directory_address_is_the_one_a_fixture_installed() {
        // The address is one process-wide value, so this test is the only one that
        // may read or replace it for as long as it holds this.
        let _exclusive = test_support::exclusive_symbol_directory().await;

        assert_eq!(symbol_directory_root().as_str(), NASDAQ_SYMBOL_DIRECTORY);

        let fixture =
            Url::parse("http://127.0.0.1:9/dynamic/SymDir/").expect("the fixture address is a URL");
        let guard = test_support::set_symbol_directory_root(fixture.clone());
        assert_eq!(
            symbol_directory_root(),
            fixture,
            "a fixture replaces the address a download dials"
        );

        drop(guard);
        assert_eq!(
            symbol_directory_root().as_str(),
            NASDAQ_SYMBOL_DIRECTORY,
            "the guard restores the address that was in place before it"
        );
    }

    /// One served directory becomes the rows both of its files state.
    #[tokio::test]
    async fn a_served_directory_becomes_its_rows() {
        let _fixture = SymbolDirectoryFixture::install(vec![
            (
                NASDAQ_LISTED_FILE,
                DirectoryAnswer::File(NASDAQ_LISTED_SAMPLE),
            ),
            (
                OTHER_LISTED_FILE,
                DirectoryAnswer::File(OTHER_LISTED_SAMPLE),
            ),
        ])
        .await;

        let rows = fetch_symbol_rows(&context())
            .await
            .expect("the fixture directory reads");

        assert_eq!(
            rows.len(),
            3,
            "both files are read and both ETF rows are not"
        );
        assert_eq!(
            rows.iter().map(|row| row.code.as_str()).collect::<Vec<_>>(),
            ["AACG", "A", "AA"],
            "each file's rows are sorted among themselves; the catalog sorts the union"
        );
    }

    /// The market participant's directory is another host, so a download carries none of the
    /// venue's headers: not the two the venue routes by, and above all not the four the
    /// signed OTP request presents.
    ///
    /// The venue's own token endpoint is signed and routed; Nasdaq's file server is
    /// neither, and it is not this build's to authenticate to. The fixture records what
    /// arrived rather than what the request was built from, so a header added to the
    /// venue's shared client — or to the signing path — is caught here.
    #[tokio::test]
    async fn the_directory_host_carries_no_venue_headers() {
        let fixture = SymbolDirectoryFixture::install(vec![
            (
                NASDAQ_LISTED_FILE,
                DirectoryAnswer::File(NASDAQ_LISTED_SAMPLE),
            ),
            (
                OTHER_LISTED_FILE,
                DirectoryAnswer::File(OTHER_LISTED_SAMPLE),
            ),
        ])
        .await;

        fetch_symbol_rows(&context())
            .await
            .expect("the fixture directory reads");

        let requests = fixture.request_headers();
        assert_eq!(requests.len(), 2, "both files were fetched from their host");
        let presented: Vec<&str> = requests
            .iter()
            .flat_map(|headers| headers.iter().map(|(name, _)| name.as_str()))
            .collect();
        for name in [
            "x-dc-region",
            "accept-language",
            "authorization",
            "x-api-key",
            "x-timestamp",
            "x-api-signature",
        ] {
            assert!(
                !presented.contains(&name),
                "the directory request carries no {name}: {presented:?}"
            );
        }
    }

    /// A directory the market participant refuses is a fetch failure, whatever the status it
    /// refused with.
    #[tokio::test]
    async fn a_refused_directory_is_a_fetch_failure() {
        for status in [404, 500] {
            let _fixture = SymbolDirectoryFixture::install(vec![(
                NASDAQ_LISTED_FILE,
                DirectoryAnswer::Status(status),
            )])
            .await;

            let error = fetch_symbol_rows(&context())
                .await
                .expect_err("a refused directory is not a directory");

            let answered = status.to_string();
            assert!(
                matches!(
                    &error,
                    MarketParticipantError::SymbolDirectoryFetchFailed { venue, reason }
                        if venue == &VenueId::Longbridge && reason.contains(&answered)
                ),
                "{error}"
            );
            assert!(error.is_retryable());
            assert!(!error.requires_user_action());
        }
    }

    /// A body above the bound is refused while it arrives rather than buffered.
    ///
    /// The fixture states a body past the bound and then holds its socket open, so
    /// the read ends at this build's own byte bound and at nothing else. The refusal
    /// has to name that bound: a reader without one would fall through to the parse
    /// of a body that is not a directory, and this assertion is what tells the two
    /// apart.
    #[tokio::test]
    async fn an_oversize_directory_is_a_fetch_failure() {
        let _fixture =
            SymbolDirectoryFixture::install(vec![(NASDAQ_LISTED_FILE, DirectoryAnswer::Oversize)])
                .await;

        let error = fetch_symbol_rows(&context())
            .await
            .expect_err("a directory larger than the bound is not read");

        let MarketParticipantError::SymbolDirectoryFetchFailed { reason, .. } = &error else {
            panic!("a directory over the bound is a fetch failure");
        };
        assert!(
            reason.contains(&SYMBOL_DIRECTORY_MAX_BYTES.to_string()),
            "the refusal names the {SYMBOL_DIRECTORY_MAX_BYTES}-byte bound it was refused by: {reason}"
        );
    }

    /// A directory that is accepted and never answered ends at the download's own
    /// deadline, and the clock of this test is the test's rather than the wall's.
    #[tokio::test(start_paused = true)]
    async fn a_directory_that_never_answers_is_a_fetch_failure() {
        let _fixture =
            SymbolDirectoryFixture::install(vec![(NASDAQ_LISTED_FILE, DirectoryAnswer::Silent)])
                .await;

        let error = fetch_symbol_rows(&context())
            .await
            .expect_err("a directory that never answers is not a directory");

        let MarketParticipantError::SymbolDirectoryFetchFailed { reason, .. } = &error else {
            panic!("a download that ends without an answer is a fetch failure");
        };
        assert!(
            reason.contains("could not be read") || reason.contains("did not answer within"),
            "a request nothing answers ends at a deadline: {reason}"
        );
    }
}
