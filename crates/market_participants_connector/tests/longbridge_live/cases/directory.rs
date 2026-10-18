//! The catalog cases: a dotted symbol, and the Nasdaq directory the venue's listings are
//! read from (§6.1–§6.4; §14.1 items 8 and 11).
//!
//! The directory cases download the market participant's own files and apply the design's filter to
//! them, so what they assert is the state of the published directory rather than this
//! build's reading of it. The parser here is the suite's own restatement of §6.2.

use bytes::Bytes;
use longbridge_proto::quote::{Direction, Period, SecurityCandlestickResponse};
use prost::Message as _;
use reqwest::Client;
use tokio::time::timeout;

use crate::{
    bounds::{CASE_TIMEOUT, HTTP_TIMEOUT},
    client::{Transport, chosen_transport, live_http_client},
    credentials::Credentials,
    skip_without_credentials,
    socket::LiveSocket,
    wire::{Failure, GET_SECURITY_HISTORY_CANDLESTICKS},
};

/// The Nasdaq symbol directory the venue's catalog is built from (§6.1).
const NASDAQ_SYMBOL_DIRECTORY: &str = "https://www.nasdaqtrader.com/dynamic/SymDir/";
const NASDAQ_LISTED_FILE: &str = "nasdaqlisted.txt";
const OTHER_LISTED_FILE: &str = "otherlisted.txt";

/// One row of one Nasdaq symbol file, before the design's filter.
struct DirectoryRow {
    code: String,
    name: String,
    market_participant: String,
    test_issue: String,
    etf: String,
    mic: &'static str,
    kept: bool,
}

/// One downloaded directory file.
struct Downloaded {
    body: Bytes,
    bytes: usize,
    rows: usize,
}

/// Downloads one symbol file, bounded by the case's own timeout.
async fn download(client: &Client, file: &'static str, case: &str) -> Downloaded {
    let url = url::Url::parse(NASDAQ_SYMBOL_DIRECTORY)
        .and_then(|base| base.join(file))
        .unwrap_or_else(|error| panic!("{case}: the directory address is malformed: {error}"));
    let response = timeout(HTTP_TIMEOUT, client.get(url).send())
        .await
        .unwrap_or_else(|_| panic!("{case}: the {file} request timed out"))
        .unwrap_or_else(|error| panic!("{case}: the {file} request failed: {error}"));
    let status = response.status();
    assert!(
        status.is_success(),
        "{case}: {file} answered {status} rather than a directory"
    );
    let body = timeout(HTTP_TIMEOUT, response.bytes())
        .await
        .unwrap_or_else(|_| panic!("{case}: the {file} body timed out"))
        .unwrap_or_else(|error| panic!("{case}: the {file} body failed: {error}"));
    let rows = body
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty())
        .count();
    Downloaded {
        bytes: body.len(),
        rows,
        body,
    }
}
/// Case 13 — a dotted symbol (§6.3; §14.1 item 8).
#[tokio::test]
#[ignore = "requires live network access to the Longbridge OpenAPI"]
async fn live_case_13_a_dotted_symbol_is_accepted_or_refused_on_the_record() {
    let case = "case 13 (dotted codes)";
    timeout(CASE_TIMEOUT, async {
        let credentials = skip_without_credentials!(case, Credentials::load());
        let mut socket =
            LiveSocket::connect(&credentials, chosen_transport(&credentials).await, true)
                .await
                .unwrap_or_else(|error| panic!("{case}: {error}"));
        socket
            .authenticate()
            .await
            .unwrap_or_else(|error| panic!("{case}: {error}"));

        let query = crate::cases::stream::offset_query(
            "BRK.B.US",
            Period::Day,
            Direction::Backward,
            "",
            "",
            5,
        );
        let outcome = socket
            .request_raw(GET_SECURITY_HISTORY_CANDLESTICKS, &query.encode_to_vec())
            .await;
        match &outcome {
            Ok(answer) => {
                let response = SecurityCandlestickResponse::decode(answer.body.as_ref());
                match response {
                    Ok(response) => println!(
                        "  BRK.B.US cmd 27: ACCEPTED (status=0) with {} candle(s) for symbol {:?}",
                        response.candlesticks.len(),
                        response.symbol
                    ),
                    Err(error) => println!(
                        "  BRK.B.US cmd 27: ACCEPTED (status=0) but the body did not decode: \
                         {error}"
                    ),
                }
            }
            Err(Failure::Refused {
                code,
                message,
                command,
            }) => println!(
                "  BRK.B.US cmd {command}: REFUSED with code {code} and the venue's own message \
                 {message:?}"
            ),
            Err(error) => panic!("{case}: {error}"),
        }

        // The control: the same command for a symbol with no dot, so a refusal of
        // the dotted one can be told apart from a broken request.
        let control = socket
            .request_raw(
                GET_SECURITY_HISTORY_CANDLESTICKS,
                &crate::cases::stream::offset_query(
                    &crate::symbols::symbol(),
                    Period::Day,
                    Direction::Backward,
                    "",
                    "",
                    5,
                )
                .encode_to_vec(),
            )
            .await;
        match &control {
            Ok(answer) => {
                let response = SecurityCandlestickResponse::decode(answer.body.as_ref());
                println!(
                    "  control {} cmd 27: ACCEPTED with {} candle(s)",
                    crate::symbols::symbol(),
                    response
                        .map(|response| response.candlesticks.len())
                        .unwrap_or(0)
                );
            }
            Err(Failure::Refused { code, message, .. }) => {
                println!(
                    "  control {} cmd 27: REFUSED code {code}: {message:?}",
                    crate::symbols::symbol()
                )
            }
            Err(error) => panic!("{case}: {error}"),
        }
        socket.shutdown().await;

        assert!(
            control.is_ok(),
            "{case}: the control read must be accepted, or the dotted result says nothing"
        );
        println!(
            "  §14.1 item 8 measured: the dotted symbol was {}",
            if outcome.is_ok() {
                "ACCEPTED"
            } else {
                "REFUSED"
            }
        );
    })
    .await
    .expect("the case must finish inside its own bound");
}

/// Case 14 — the Nasdaq directory (§6.1–§6.4).
#[tokio::test]
#[ignore = "requires live network access to the Longbridge OpenAPI"]
async fn live_case_14_the_nasdaq_directory_downloads_and_filters_as_designed() {
    let case = "case 14 (directory)";
    timeout(CASE_TIMEOUT, async {
        // The directory belongs to the market participant that publishes it, not to the venue,
        // so this case needs no credential — but it still takes the run's own route,
        // which the first case to hold credentials establishes.
        let transport = match Credentials::load() {
            Some(credentials) => chosen_transport(&credentials).await,
            None => Transport::Direct,
        };
        let client = live_http_client(transport).expect("the live HTTP client builds");
        println!("{case}: transport={transport:?}");
        let nasdaq = download(&client, NASDAQ_LISTED_FILE, case).await;
        let other = download(&client, OTHER_LISTED_FILE, case).await;
        println!(
            "  nasdaqlisted.txt: {} bytes, {} data rows",
            nasdaq.bytes, nasdaq.rows
        );
        println!(
            "  otherlisted.txt: {} bytes, {} data rows",
            other.bytes, other.rows
        );

        let nasdaq_rows = parse_directory(NASDAQ_LISTED_FILE, &nasdaq.body, true);
        let other_rows = parse_directory(OTHER_LISTED_FILE, &other.body, false);
        let nasdaq_filtered = nasdaq_rows.iter().filter(|row| row.kept).count();
        let other_filtered = other_rows.iter().filter(|row| row.kept).count();
        println!(
            "  after the design's filter: nasdaqlisted={nasdaq_filtered} row(s) (expected ≈4329), \
             otherlisted={other_filtered} row(s) (expected ≈3160)"
        );
        let dropped_tests = nasdaq_rows
            .iter()
            .chain(other_rows.iter())
            .filter(|row| row.test_issue == "Y")
            .count();
        let dropped_etfs = nasdaq_rows
            .iter()
            .chain(other_rows.iter())
            .filter(|row| row.etf == "Y")
            .count();
        println!("  dropped: {dropped_etfs} ETF row(s), {dropped_tests} Test Issue row(s)");

        let f_rows: Vec<&DirectoryRow> = other_rows
            .iter()
            .filter(|row| row.market_participant == "F" && row.kept)
            .collect();
        println!(
            "  otherlisted Exchange=F rows kept: {} -> {:?}",
            f_rows.len(),
            f_rows
                .iter()
                .map(|row| row.code.as_str())
                .collect::<Vec<_>>()
        );
        let dds = other_rows
            .iter()
            .find(|row| row.code == "DDS")
            .unwrap_or_else(|| panic!("{case}: the F row for DDS must survive the filter"));
        println!(
            "  DDS: market_participant={:?} mic={} kept={} test_issue={:?} etf={:?} name={:?}",
            dds.market_participant, dds.mic, dds.kept, dds.test_issue, dds.etf, dds.name
        );
        assert_eq!(
            dds.market_participant, "F",
            "{case}: DDS is published on the undocumented F exchange (§6.4)"
        );
        assert_eq!(
            dds.mic, "TXSE",
            "{case}: F maps to TXSE — Texas Stock Exchange, LLC (§6.4)"
        );
        assert!(
            dds.kept,
            "{case}: the F row survives the filter, which is why the mapping exists (§6.4)"
        );

        let m_rows: Vec<&DirectoryRow> = other_rows
            .iter()
            .filter(|row| row.market_participant == "M")
            .collect();
        println!(
            "  otherlisted Exchange=M rows: {} present, {} kept (the design drops them)",
            m_rows.len(),
            m_rows.iter().filter(|row| row.kept).count()
        );
        for row in &m_rows {
            println!(
                "    M row: code={:?} name={:?} test_issue={:?} etf={:?} kept={}",
                row.code, row.name, row.test_issue, row.etf, row.kept
            );
        }
        assert!(
            !m_rows.is_empty(),
            "{case}: the venue's directory still publishes M rows, which the design drops"
        );
        assert!(
            m_rows.iter().all(|row| !row.kept),
            "{case}: an M row must never be published, because its MIC is undecided (§6.4)"
        );
        assert!(
            m_rows.iter().all(|row| row.test_issue == "Y"),
            "{case}: the design says today's M rows are all Test Issue rows (§14.2 item 11)"
        );

        assert!(
            (4_200..=4_450).contains(&nasdaq_filtered),
            "{case}: nasdaqlisted filters to ≈4329 rows, measured {nasdaq_filtered}"
        );
        assert!(
            (3_050..=3_300).contains(&other_filtered),
            "{case}: otherlisted filters to ≈3160 rows, measured {other_filtered}"
        );
        assert!(
            nasdaq_rows.iter().any(|row| row.code == "AAPL" && row.kept),
            "{case}: AAPL is a Nasdaq-listed security and survives the filter"
        );
        assert!(
            nasdaq_rows.iter().all(|row| !row.kept || row.mic == "XNAS"),
            "{case}: every published nasdaqlisted row is listed on XNAS (§6.4)"
        );
    })
    .await
    .expect("the case must finish inside its own bound");
}
// ---------------------------------------------------------------------------
// Directory parsing (§6.1–§6.4)
// ---------------------------------------------------------------------------

/// One row of one Nasdaq symbol file, before the design's filter.
fn parse_directory(file: &str, body: &[u8], is_nasdaq: bool) -> Vec<DirectoryRow> {
    const NASDAQ_HEADER: &str = "Symbol|Security Name|Market Category|Test Issue|Financial Status|Round Lot Size|ETF|NextShares";
    const OTHER_HEADER: &str =
        "ACT Symbol|Security Name|Exchange|CQS Symbol|ETF|Round Lot Size|Test Issue|NASDAQ Symbol";
    let expected_header = if is_nasdaq {
        NASDAQ_HEADER
    } else {
        OTHER_HEADER
    };
    let mut rows = Vec::with_capacity(9_000);
    let mut lines = body.split(|byte| *byte == b'\n');
    let header = lines
        .next()
        .unwrap_or_else(|| panic!("the {file} directory must start with its header"));
    let header = String::from_utf8_lossy(header);
    assert_eq!(
        header.trim_end_matches('\r'),
        expected_header,
        "the {file} header must be the one the design states (§6.1)"
    );
    for line in lines {
        let line = String::from_utf8_lossy(line);
        let line = line.trim_end_matches('\r');
        if line.is_empty() {
            continue;
        }
        if line.starts_with("File Creation Time:") {
            break;
        }
        let fields: Vec<&str> = line.split('|').collect();
        assert_eq!(
            fields.len(),
            8,
            "a {file} data row must carry eight fields (§6.2 step 2)"
        );
        let (code, name, market_participant, test_issue, etf) = if is_nasdaq {
            (fields[0], fields[1], "N", fields[3], fields[6])
        } else {
            (fields[0], fields[1], fields[2], fields[6], fields[4])
        };
        let mic = match market_participant {
            "N" if !is_nasdaq => "XNYS",
            "A" => "XASE",
            "P" => "ARCX",
            "Z" => "BATS",
            "V" => "IEXG",
            "F" => "TXSE",
            _ if is_nasdaq => "XNAS",
            _ => "",
        };
        let kept = test_issue != "Y"
            && etf != "Y"
            && !code.trim().is_empty()
            && !name.trim().is_empty()
            && !mic.is_empty();
        rows.push(DirectoryRow {
            code: code.to_owned(),
            name: name.trim().to_owned(),
            market_participant: market_participant.to_owned(),
            test_issue: test_issue.to_owned(),
            etf: etf.to_owned(),
            mic,
            kept,
        });
    }
    rows
}
