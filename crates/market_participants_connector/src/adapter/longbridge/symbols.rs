use std::{str, time::Duration};

use bytes::{Bytes, BytesMut};
use futures::StreamExt as _;
use market_participants_model::identifier::{Mic, VenueId};
use smol_str::{SmolStr, format_smolstr};
use url::Url;

use crate::{
    adapter::ExecutionContext, catalog::resolved::ResolvedListingRef, error::MarketParticipantError,
};

pub(crate) const NASDAQ_SYMBOL_DIRECTORY: &str = "https://www.nasdaqtrader.com/dynamic/SymDir/";
pub(crate) const NASDAQ_LISTED_FILE: &str = "nasdaqlisted.txt";
pub(crate) const OTHER_LISTED_FILE: &str = "otherlisted.txt";

pub(crate) const SYMBOL_DIRECTORY_MAX_BYTES: usize = 16 * 1024 * 1024;

pub(crate) const SYMBOL_DIRECTORY_TIMEOUT: Duration = Duration::from_secs(30);

const NASDAQ_LISTED_HEADER: &str = "Symbol|Security Name|Market Category|Test Issue|Financial Status|Round Lot Size|ETF|NextShares";

const OTHER_LISTED_HEADER: &str =
    "ACT Symbol|Security Name|Exchange|CQS Symbol|ETF|Round Lot Size|Test Issue|NASDAQ Symbol";

const NASDAQ_LISTED_MIC: &str = "XNAS";

pub(super) const MARKET_SUFFIX: &str = "US";

const FILE_CREATION_TIME: &str = "File Creation Time";

const MAX_CODE_LENGTH: usize = 14;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct SymbolRow {
    pub code: SmolStr,
    pub name: SmolStr,
    pub mic: Mic,
}

#[derive(Clone, Copy)]
struct DirectoryLayout {
    header: &'static str,
    columns: usize,
    code: usize,
    name: usize,
    test_issue: usize,
    etf: usize,
    market: MarketColumn,
}

#[derive(Clone, Copy)]
enum MarketColumn {
    Whole(&'static str),
    PerRow(usize),
}

const NASDAQ_LISTED_LAYOUT: DirectoryLayout = DirectoryLayout {
    header: NASDAQ_LISTED_HEADER,
    columns: 8,
    code: 0,
    name: 1,
    test_issue: 3,
    etf: 6,
    market: MarketColumn::Whole(NASDAQ_LISTED_MIC),
};

const OTHER_LISTED_LAYOUT: DirectoryLayout = DirectoryLayout {
    header: OTHER_LISTED_HEADER,
    columns: 8,
    code: 0,
    name: 1,
    test_issue: 6,
    etf: 4,
    market: MarketColumn::PerRow(2),
};

const MARKET_PARTICIPANT_MICS: [(&str, &str); 6] = [
    ("N", "XNYS"),
    ("A", "XASE"),
    ("P", "ARCX"),
    ("Z", "BATS"),
    ("V", "IEXG"),
    ("F", "TXSE"),
];

pub(crate) async fn fetch_symbol_rows(
    context: &ExecutionContext,
) -> Result<Vec<SymbolRow>, MarketParticipantError> {
    let mut rows = Vec::with_capacity(12_000);
    rows.extend(download_and_parse(context, NASDAQ_LISTED_FILE).await?);
    rows.extend(download_and_parse(context, OTHER_LISTED_FILE).await?);
    Ok(rows)
}

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

fn layout_of(file: &str) -> Result<DirectoryLayout, MarketParticipantError> {
    match file {
        NASDAQ_LISTED_FILE => Ok(NASDAQ_LISTED_LAYOUT),
        OTHER_LISTED_FILE => Ok(OTHER_LISTED_LAYOUT),
        _ => Err(symbol_directory_failed(format_smolstr!(
            "{file} is not a symbol directory this build reads"
        ))),
    }
}

fn market_mic(market: MarketColumn, fields: &[&str]) -> Option<Mic> {
    let code = match market {
        MarketColumn::Whole(mic) => mic,
        MarketColumn::PerRow(column) => MARKET_PARTICIPANT_MICS
            .iter()
            .find(|(market_participant, _)| *market_participant == fields[column])
            .map(|(_, mic)| *mic)?,
    };
    Some(
        Mic::new(code)
            .expect("the market-participant table names canonical market identifier codes"),
    )
}

#[cfg(not(feature = "test-support"))]
fn symbol_directory_root() -> Url {
    Url::parse(NASDAQ_SYMBOL_DIRECTORY).expect("the published directory address is a URL")
}

#[cfg(feature = "test-support")]
fn symbol_directory_root() -> Url {
    crate::test_support::symbol_directory_root(NASDAQ_SYMBOL_DIRECTORY)
}

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

    fn route() -> VenueRoute {
        VenueRoute::new(
            VenueId::Longbridge,
            VenueScopeId::new(LONGBRIDGE_US_SCOPE).expect("the US scope is canonical"),
        )
    }

    fn context() -> crate::adapter::ExecutionContext {
        test_support::loopback_execution(route(), CapabilityId::MarketCatalog)
    }

    fn other_listed(rows: &[&str]) -> String {
        format!("{OTHER_LISTED_HEADER}\r\n{}\r\n", rows.join("\r\n"))
    }

    fn nasdaq_listed(rows: &[&str]) -> String {
        format!("{NASDAQ_LISTED_HEADER}\r\n{}\r\n", rows.join("\r\n"))
    }

    fn codes(file: &str, body: &str) -> Vec<String> {
        parse(file, body.as_bytes())
            .expect("the fixture file parses")
            .into_iter()
            .map(|row| row.code.to_string())
            .collect()
    }

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

    #[test]
    fn both_line_endings_parse_the_same_file() {
        let windows = OTHER_LISTED_SAMPLE;
        let unix = windows.replace("\r\n", "\n");

        assert_eq!(
            parse(OTHER_LISTED_FILE, unix.as_bytes()).expect("the unix sample parses"),
            parse(OTHER_LISTED_FILE, windows.as_bytes()).expect("the windows sample parses")
        );
    }

    #[test]
    fn the_rows_are_sorted_and_deduplicated_by_code() {
        let other = other_listed(&[
            "ZZZ|The Last One|N|ZZZ|N|100|N|ZZZ",
            "AAA|The First One|N|AAA|N|100|N|AAA",
            "AAA|The First One Again|N|AAA|N|100|N|AAA",
        ]);

        assert_eq!(codes(OTHER_LISTED_FILE, &other), ["AAA", "ZZZ"]);
    }

    #[tokio::test]
    async fn the_directory_address_is_the_one_a_fixture_installed() {
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
