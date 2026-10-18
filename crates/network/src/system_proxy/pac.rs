//! PAC script download, JavaScript evaluation, and process cache.
//!
//! This path serves the Linux and macOS system proxy implementations. Windows
//! evaluates PAC through WinHTTP instead, so no JavaScript engine is involved
//! there.

use std::{
    io::Read,
    sync::{Arc, Condvar, LazyLock, Mutex},
    thread,
    time::{Duration, Instant},
};

use ahash::AHashMap;
use app_config::network::{ConfigError, ResolvedProxy, RouteTarget};
use bytes::BytesMut;
use url::Url;

use super::{core::pac_request_url, parse::parse_pac_decision};
use crate::error::NetworkError;

const PAC_TIMEOUT: Duration = Duration::from_secs(10);
pub(super) const PAC_MAX_BYTES: usize = 1024 * 1024;
const PAC_CACHE_TTL: Duration = Duration::from_secs(3600);
const GLOB_MAX_STATES: usize = PAC_MAX_BYTES * 8;
const PAC_DNS_TIMEOUT: Duration = Duration::from_secs(1);
const PAC_DNS_TOTAL_TIMEOUT: Duration = Duration::from_secs(2);
const PAC_DNS_CACHE_TTL: Duration = Duration::from_secs(10);
pub(super) const PAC_DNS_MAX_CALLS: usize = 32;

struct PacCache {
    fingerprint: u64,
    script_url: String,
    body: String,
    loaded_at: Instant,
}

static PAC_CACHE: Mutex<Option<PacCache>> = Mutex::new(None);

#[derive(Default)]
struct DnsLookupState {
    running: bool,
    cached: Option<(String, Option<String>, Instant)>,
}

pub(super) struct DnsLookupCoordinator {
    state: Mutex<DnsLookupState>,
    ready: Condvar,
}

impl DnsLookupCoordinator {
    pub(super) fn new() -> Self {
        Self {
            state: Mutex::new(DnsLookupState::default()),
            ready: Condvar::new(),
        }
    }
}

static DNS_LOOKUPS: LazyLock<Arc<DnsLookupCoordinator>> =
    LazyLock::new(|| Arc::new(DnsLookupCoordinator::new()));

pub(super) struct PacDnsBudget {
    pub calls: usize,
    pub deadline: Instant,
    pub cache: AHashMap<String, Option<String>>,
}

impl PacDnsBudget {
    fn new() -> Self {
        Self {
            calls: 0,
            deadline: Instant::now() + PAC_DNS_TOTAL_TIMEOUT,
            cache: AHashMap::with_capacity(PAC_DNS_MAX_CALLS),
        }
    }

    pub(super) fn next_timeout(&mut self, now: Instant) -> Option<Duration> {
        if self.calls >= PAC_DNS_MAX_CALLS {
            return None;
        }
        let remaining = self.deadline.checked_duration_since(now)?;
        if remaining.is_zero() {
            return None;
        }
        self.calls += 1;
        Some(remaining.min(PAC_DNS_TIMEOUT))
    }
}

thread_local! {
    static PAC_DNS_BUDGET: std::cell::RefCell<Option<PacDnsBudget>> = const { std::cell::RefCell::new(None) };
}

pub(super) fn evaluate_pac_url(
    script_url: &str,
    destination: &Url,
    fingerprint: u64,
    target: &RouteTarget,
) -> Result<ResolvedProxy, ConfigError> {
    let script_url = script_url.trim();
    if script_url.is_empty() {
        return Err(ConfigError::InvalidSystemProxy {
            target: target.clone(),
        });
    }
    let parsed = Url::parse(script_url).map_err(|_| ConfigError::InvalidSystemProxy {
        target: target.clone(),
    })?;
    if parsed.scheme() != "http" && parsed.scheme() != "https" {
        return Err(ConfigError::InvalidSystemProxy {
            target: target.clone(),
        });
    }
    let body = load_pac_script(&parsed, fingerprint, target)?;
    evaluate_pac_script(&body, destination, target)
}

pub(super) fn evaluate_pac_script(
    script: &str,
    destination: &Url,
    target: &RouteTarget,
) -> Result<ResolvedProxy, ConfigError> {
    let url = pac_request_url(destination);
    let host = url.host_str().ok_or(ConfigError::InvalidSystemProxy {
        target: target.clone(),
    })?;
    let decision = find_proxy_for_url(script, url.as_str(), host).map_err(|_| {
        ConfigError::InvalidSystemProxy {
            target: target.clone(),
        }
    })?;
    parse_pac_decision(&decision, target)
}

fn load_pac_script(
    script_url: &Url,
    fingerprint: u64,
    target: &RouteTarget,
) -> Result<String, ConfigError> {
    {
        let cache = PAC_CACHE
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(cache) = cache.as_ref()
            && cache.fingerprint == fingerprint
            && cache.script_url == script_url.as_str()
            && cache.loaded_at.elapsed() < PAC_CACHE_TTL
        {
            return Ok(cache.body.clone());
        }
    }
    let body = fetch_pac_script(script_url).map_err(|_| ConfigError::InvalidSystemProxy {
        target: target.clone(),
    })?;
    let mut cache = PAC_CACHE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    *cache = Some(PacCache {
        fingerprint,
        script_url: script_url.to_string(),
        body: body.clone(),
        loaded_at: Instant::now(),
    });
    Ok(body)
}

fn fetch_pac_script(url: &Url) -> Result<String, NetworkError> {
    let url = url.clone();
    std::thread::Builder::new()
        .name("kytrix-pac-fetch".into())
        .spawn(move || {
            let client = reqwest::blocking::Client::builder()
                .no_proxy()
                .timeout(PAC_TIMEOUT)
                .connect_timeout(PAC_TIMEOUT)
                .redirect(reqwest::redirect::Policy::limited(3))
                .build()
                .map_err(|error| NetworkError::Io {
                    message: smol_str::format_smolstr!("{error}"),
                })?;
            let response = client.get(url).send().map_err(|error| NetworkError::Io {
                message: smol_str::format_smolstr!("{error}"),
            })?;
            if !response.status().is_success() {
                return Err(NetworkError::InvalidData {
                    operation: "fetch PAC script".into(),
                    reason: smol_str::format_smolstr!("HTTP status {}", response.status()),
                });
            }
            if response
                .content_length()
                .is_some_and(|size| size > PAC_MAX_BYTES as u64)
            {
                return Err(NetworkError::LimitExceeded {
                    operation: "fetch PAC script".into(),
                    limit: PAC_MAX_BYTES,
                });
            }
            read_bounded_pac_body(response)
        })
        .map_err(|error| NetworkError::Io {
            message: smol_str::format_smolstr!("{error}"),
        })?
        .join()
        .map_err(|_| NetworkError::Io {
            message: "PAC fetch worker panicked".into(),
        })?
}

/// Reads a PAC document, refusing anything past the script size limit.
pub(super) fn read_bounded_pac_body(mut reader: impl Read) -> Result<String, NetworkError> {
    let mut body = BytesMut::with_capacity(8192);
    let mut buffer = [0_u8; 8192];
    loop {
        let count = reader.read(&mut buffer).map_err(|error| NetworkError::Io {
            message: smol_str::format_smolstr!("{error}"),
        })?;
        if count == 0 {
            break;
        }
        if body.len().saturating_add(count) > PAC_MAX_BYTES {
            return Err(NetworkError::LimitExceeded {
                operation: "read PAC script".into(),
                limit: PAC_MAX_BYTES,
            });
        }
        body.extend_from_slice(&buffer[..count]);
    }
    std::str::from_utf8(body.as_ref())
        .map(str::to_owned)
        .map_err(|_| NetworkError::InvalidData {
            operation: "read PAC script".into(),
            reason: "body is not valid UTF-8".into(),
        })
}

fn find_proxy_for_url(script: &str, url: &str, host: &str) -> Result<String, NetworkError> {
    PAC_DNS_BUDGET.with(|cell| *cell.borrow_mut() = Some(PacDnsBudget::new()));
    let result = evaluate_find_proxy_for_url(script, url, host);
    PAC_DNS_BUDGET.with(|cell| *cell.borrow_mut() = None);
    result
}

fn evaluate_find_proxy_for_url(
    script: &str,
    url: &str,
    host: &str,
) -> Result<String, NetworkError> {
    use boa_engine::{Context, JsArgs, JsResult, JsValue, NativeFunction, Source, js_string};

    fn js_arg_string(args: &[JsValue], index: usize, context: &mut Context) -> JsResult<String> {
        args.get_or_undefined(index)
            .to_string(context)?
            .to_std_string()
            .map_err(|_| boa_engine::JsError::from_opaque(js_string!("invalid string").into()))
    }

    fn sh_exp_match(_this: &JsValue, args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
        let value = js_arg_string(args, 0, context)?;
        let pattern = js_arg_string(args, 1, context)?;
        let matched = sh_exp_match_glob(&value, &pattern).map_err(|error| {
            boa_engine::JsError::from_opaque(js_string!(error.to_string()).into())
        })?;
        Ok(JsValue::from(matched))
    }

    fn dns_domain_is(
        _this: &JsValue,
        args: &[JsValue],
        context: &mut Context,
    ) -> JsResult<JsValue> {
        let host = js_arg_string(args, 0, context)?;
        let domain = js_arg_string(args, 1, context)?;
        Ok(JsValue::from(dns_domain_matches(&host, &domain)))
    }

    fn is_plain_host_name(
        _this: &JsValue,
        args: &[JsValue],
        context: &mut Context,
    ) -> JsResult<JsValue> {
        let host = js_arg_string(args, 0, context)?;
        Ok(JsValue::from(!host.contains('.')))
    }

    fn dns_resolve(_this: &JsValue, args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
        let host = js_arg_string(args, 0, context)?;
        Ok(match resolve_host_ip_for_pac(&host) {
            Some(ip) => JsValue::from(js_string!(ip.as_str())),
            None => JsValue::from(js_string!("")),
        })
    }

    fn is_resolvable(
        _this: &JsValue,
        args: &[JsValue],
        context: &mut Context,
    ) -> JsResult<JsValue> {
        let host = js_arg_string(args, 0, context)?;
        Ok(JsValue::from(resolve_host_ip_for_pac(&host).is_some()))
    }

    fn my_ip_address(
        _this: &JsValue,
        _args: &[JsValue],
        _context: &mut Context,
    ) -> JsResult<JsValue> {
        Ok(JsValue::from(js_string!(local_ip().as_str())))
    }

    fn is_in_net(_this: &JsValue, args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
        let host = js_arg_string(args, 0, context)?;
        let pattern = js_arg_string(args, 1, context)?;
        let mask = js_arg_string(args, 2, context)?;
        Ok(JsValue::from(host_in_net(&host, &pattern, &mask)))
    }

    fn dns_domain_levels(
        _this: &JsValue,
        args: &[JsValue],
        context: &mut Context,
    ) -> JsResult<JsValue> {
        let host = js_arg_string(args, 0, context)?;
        let levels = host.bytes().filter(|byte| *byte == b'.').count() as i32;
        Ok(JsValue::from(levels))
    }

    fn local_host_or_domain_is(
        _this: &JsValue,
        args: &[JsValue],
        context: &mut Context,
    ) -> JsResult<JsValue> {
        let host = js_arg_string(args, 0, context)?;
        let hostdom = js_arg_string(args, 1, context)?;
        Ok(JsValue::from(local_host_or_domain_matches(&host, &hostdom)))
    }

    fn alert(_this: &JsValue, _args: &[JsValue], _context: &mut Context) -> JsResult<JsValue> {
        Ok(JsValue::undefined())
    }

    const PAC_LOOP_LIMIT: u64 = 100_000;
    const PAC_RECURSION_LIMIT: usize = 64;

    let mut context = Context::default();
    context
        .runtime_limits_mut()
        .set_loop_iteration_limit(PAC_LOOP_LIMIT);
    context
        .runtime_limits_mut()
        .set_recursion_limit(PAC_RECURSION_LIMIT);
    let register = |context: &mut Context, name: &str, length: usize, function: NativeFunction| {
        context
            .register_global_callable(js_string!(name.to_owned()), length, function)
            .map_err(|_| NetworkError::InvalidData {
                operation: "register PAC helper".into(),
                reason: "JavaScript helper registration failed".into(),
            })
    };
    register(
        &mut context,
        "shExpMatch",
        2,
        NativeFunction::from_fn_ptr(sh_exp_match),
    )?;
    register(
        &mut context,
        "dnsDomainIs",
        2,
        NativeFunction::from_fn_ptr(dns_domain_is),
    )?;
    register(
        &mut context,
        "isPlainHostName",
        1,
        NativeFunction::from_fn_ptr(is_plain_host_name),
    )?;
    register(
        &mut context,
        "dnsResolve",
        1,
        NativeFunction::from_fn_ptr(dns_resolve),
    )?;
    register(
        &mut context,
        "isResolvable",
        1,
        NativeFunction::from_fn_ptr(is_resolvable),
    )?;
    register(
        &mut context,
        "myIpAddress",
        0,
        NativeFunction::from_fn_ptr(my_ip_address),
    )?;
    register(
        &mut context,
        "isInNet",
        3,
        NativeFunction::from_fn_ptr(is_in_net),
    )?;
    register(
        &mut context,
        "dnsDomainLevels",
        1,
        NativeFunction::from_fn_ptr(dns_domain_levels),
    )?;
    register(
        &mut context,
        "localHostOrDomainIs",
        2,
        NativeFunction::from_fn_ptr(local_host_or_domain_is),
    )?;
    register(&mut context, "alert", 1, NativeFunction::from_fn_ptr(alert))?;

    context
        .eval(Source::from_bytes(script.as_bytes()))
        .map_err(|_| NetworkError::InvalidData {
            operation: "evaluate PAC script".into(),
            reason: "script evaluation failed".into(),
        })?;
    let call = format!("FindProxyForURL({}, {})", js_quote(url), js_quote(host));
    let value = context
        .eval(Source::from_bytes(call.as_bytes()))
        .map_err(|_| NetworkError::InvalidData {
            operation: "evaluate PAC script".into(),
            reason: "FindProxyForURL evaluation failed".into(),
        })?;
    value
        .to_string(&mut context)
        .map_err(|_| NetworkError::InvalidData {
            operation: "evaluate PAC script".into(),
            reason: "proxy decision conversion failed".into(),
        })?
        .to_std_string()
        .map_err(|_| NetworkError::InvalidData {
            operation: "evaluate PAC script".into(),
            reason: "proxy decision is not valid UTF-8".into(),
        })
}

fn js_quote(value: &str) -> String {
    format!("\"{}\"", value.replace('\\', "\\\\").replace('"', "\\\""))
}

pub(super) fn sh_exp_match_glob(value: &str, pattern: &str) -> Result<bool, NetworkError> {
    glob_match(value.as_bytes(), pattern.as_bytes())
}

fn glob_match(value: &[u8], pattern: &[u8]) -> Result<bool, NetworkError> {
    if value.len() > PAC_MAX_BYTES || pattern.len() > PAC_MAX_BYTES {
        return Err(NetworkError::LimitExceeded {
            operation: "PAC glob match input".into(),
            limit: PAC_MAX_BYTES,
        });
    }
    let states = value
        .len()
        .checked_add(1)
        .and_then(|rows| {
            pattern
                .len()
                .checked_add(1)
                .and_then(|columns| rows.checked_mul(columns))
        })
        .ok_or_else(|| NetworkError::LimitExceeded {
            operation: "PAC glob match".into(),
            limit: GLOB_MAX_STATES,
        })?;
    if states > GLOB_MAX_STATES {
        return Err(NetworkError::LimitExceeded {
            operation: "PAC glob match".into(),
            limit: GLOB_MAX_STATES,
        });
    }

    let mut previous = vec![false; pattern.len() + 1];
    previous[0] = true;
    for (index, byte) in pattern.iter().enumerate() {
        previous[index + 1] = *byte == b'*' && previous[index];
    }

    let mut current = vec![false; pattern.len() + 1];
    for value_byte in value {
        for (index, pattern_byte) in pattern.iter().enumerate() {
            current[index + 1] = match pattern_byte {
                b'*' => current[index] || previous[index + 1],
                b'?' => previous[index],
                byte => *byte == *value_byte && previous[index],
            };
        }
        std::mem::swap(&mut previous, &mut current);
        current.fill(false);
    }
    Ok(previous[pattern.len()])
}

pub(super) fn dns_domain_matches(host: &str, domain: &str) -> bool {
    !host.is_empty() && !domain.is_empty() && host.ends_with(domain)
}

pub(super) fn local_host_or_domain_matches(host: &str, hostdom: &str) -> bool {
    host == hostdom || (!host.contains('.') && hostdom.starts_with(&format!("{host}.")))
}

fn resolve_host_ip_for_pac(host: &str) -> Option<String> {
    let host = host.trim();
    if host.is_empty() {
        return None;
    }
    PAC_DNS_BUDGET.with(|cell| {
        let mut budget = cell.borrow_mut();
        if let Some(cached) = budget.as_ref().and_then(|budget| budget.cache.get(host)) {
            return cached.clone();
        }
        let timeout = budget.as_mut().map_or(Some(PAC_DNS_TIMEOUT), |budget| {
            budget.next_timeout(Instant::now())
        })?;
        drop(budget);
        let resolved = bounded_dns_lookup(host.to_owned(), timeout);
        let mut budget = cell.borrow_mut();
        if let Some(budget) = budget.as_mut() {
            budget.cache.insert(host.to_owned(), resolved.clone());
        }
        resolved
    })
}

fn bounded_dns_lookup(host: String, timeout: Duration) -> Option<String> {
    bounded_dns_lookup_with(Arc::clone(&DNS_LOOKUPS), host, timeout, lookup_host_ip)
}

pub(super) fn bounded_dns_lookup_with(
    coordinator: Arc<DnsLookupCoordinator>,
    host: String,
    timeout: Duration,
    discover: impl FnOnce(&str) -> Option<String> + Send + 'static,
) -> Option<String> {
    let deadline = Instant::now() + timeout;
    let mut state = coordinator
        .state
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some((cached_host, result, completed_at)) = state.cached.as_ref()
        && cached_host == &host
        && completed_at.elapsed() < PAC_DNS_CACHE_TTL
    {
        return result.clone();
    }
    if state.running {
        loop {
            let remaining = deadline.checked_duration_since(Instant::now())?;
            let (next, wait) = coordinator
                .ready
                .wait_timeout(state, remaining)
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            state = next;
            if !state.running {
                return state
                    .cached
                    .as_ref()
                    .filter(|(cached_host, _, completed_at)| {
                        cached_host == &host && completed_at.elapsed() < PAC_DNS_CACHE_TTL
                    })
                    .and_then(|(_, result, _)| result.clone());
            }
            if wait.timed_out() {
                return None;
            }
        }
    }

    state.running = true;
    drop(state);
    let worker_coordinator = Arc::clone(&coordinator);
    let worker_host = host.clone();
    let worker = thread::Builder::new()
        .name("kytrix-pac-dns".into())
        .spawn(move || {
            let result = discover(&worker_host);
            let mut state = worker_coordinator
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            state.running = false;
            state.cached = Some((worker_host, result, Instant::now()));
            drop(state);
            worker_coordinator.ready.notify_all();
        });
    if worker.is_err() {
        let mut state = coordinator
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.running = false;
        state.cached = Some((host, None, Instant::now()));
        drop(state);
        coordinator.ready.notify_all();
        return None;
    }

    let mut state = coordinator
        .state
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    loop {
        if !state.running {
            return state
                .cached
                .as_ref()
                .filter(|(cached_host, _, completed_at)| {
                    cached_host == &host && completed_at.elapsed() < PAC_DNS_CACHE_TTL
                })
                .and_then(|(_, result, _)| result.clone());
        }
        let remaining = deadline.checked_duration_since(Instant::now())?;
        let (next, wait) = coordinator
            .ready
            .wait_timeout(state, remaining)
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state = next;
        if wait.timed_out() && state.running {
            return None;
        }
    }
}

fn lookup_host_ip(host: &str) -> Option<String> {
    use std::net::ToSocketAddrs;
    format!("{host}:0")
        .to_socket_addrs()
        .ok()?
        .next()
        .map(|address| address.ip().to_string())
}

fn local_ip() -> String {
    std::net::UdpSocket::bind("0.0.0.0:0")
        .and_then(|socket| {
            socket.connect("8.8.8.8:80")?;
            Ok(socket.local_addr()?.ip().to_string())
        })
        .unwrap_or_else(|_| "127.0.0.1".into())
}

fn host_in_net(host: &str, pattern: &str, mask: &str) -> bool {
    let host_ip = resolve_host_ip_for_pac(host)
        .or_else(|| Some(host.to_owned()))
        .and_then(|value| value.parse::<std::net::Ipv4Addr>().ok());
    let pattern_ip = pattern.parse::<std::net::Ipv4Addr>().ok();
    let mask_ip = mask.parse::<std::net::Ipv4Addr>().ok();
    match (host_ip, pattern_ip, mask_ip) {
        (Some(host), Some(pattern), Some(mask)) => {
            u32::from(host) & u32::from(mask) == u32::from(pattern) & u32::from(mask)
        }
        _ => false,
    }
}
