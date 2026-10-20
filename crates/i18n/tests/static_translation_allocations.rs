//! Pins what the static fast path buys: a message without placeholders is stored
//! as the exact text callers receive, so translating it again is a clone of an
//! already-built string and costs no allocation at all.
//!
//! Text and address assertions elsewhere in this crate also hold when a static
//! message is rendered and cached like an interpolated one. This test counts the
//! allocations of the calls themselves, so deleting the fast path fails it.

use std::{
    alloc::{GlobalAlloc, Layout, System},
    cell::Cell,
    hint::black_box,
    sync::{
        Mutex,
        atomic::{AtomicUsize, Ordering},
    },
};

use i18n::{language::codes::LanguageCode, translator::types::I18n};

/// Translations per measured window: enough that one allocation per call cannot
/// hide, few enough that the test stays instant.
const CALLS: usize = 1000;

/// Distinct argument values in the control window. The render cache holds 64
/// entries, so ten values never evict anything and every value is a genuine miss
/// that has to be rendered.
const DISTINCT_VALUES: i32 = 10;

/// Longer than the 23 bytes a short string keeps inline, so its clone bumps a
/// reference count instead of copying a buffer.
const LONG_MESSAGE: &str = "The stored sign-in credential is invalid";

const EN: &str = r#"
menu:
  market: Market
  portfolio: Portfolio
credential: The stored sign-in credential is invalid
notice: The stored sign-in credential expired yesterday
price: "%{amount} USD"
"#;

/// Wraps [`System`] and counts calls to `alloc` and `realloc`. Frees are not
/// counted, because only the work a translation does is measured.
struct CountingAllocator;

/// Allocations made by whichever thread currently has [`COUNTING`] set.
static ALLOCATIONS: AtomicUsize = AtomicUsize::new(0);

/// Serialises measured windows, so two tests in this binary cannot count each
/// other's allocations.
static MEASURING: Mutex<()> = Mutex::new(());

thread_local! {
    /// Whether this thread is inside a measured window.
    ///
    /// A `const`-initialised `Cell` rather than a lazily initialised value:
    /// reading it does not allocate, take a lock or register a destructor, so
    /// the counter cannot recurse into itself or leak into another test's run.
    static COUNTING: Cell<bool> = const { Cell::new(false) };
}

// SAFETY: every method forwards to `System` with the layout and pointer it was
// given, so allocation, reallocation and deallocation keep their contracts.
unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        if COUNTING.with(Cell::get) {
            ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
        }

        // SAFETY: the caller guarantees `layout` is valid.
        unsafe { System.alloc(layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        if COUNTING.with(Cell::get) {
            ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
        }

        // SAFETY: the caller guarantees `ptr` and `layout` are valid and that
        // `new_size` describes the new allocation.
        unsafe { System.realloc(ptr, layout, new_size) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: the caller guarantees `ptr` came from this allocator with
        // `layout`; only the work of acquiring memory is measured, so frees are
        // forwarded without being counted.
        unsafe { System.dealloc(ptr, layout) }
    }
}

#[global_allocator]
static ALLOCATOR: CountingAllocator = CountingAllocator;

/// Returns the number of allocations `measure` made on this thread.
///
/// Counting is per thread, so a test running alongside this one is never
/// attributed to the window, and the lock keeps two windows from overlapping.
fn allocations_during(measure: impl FnOnce()) -> usize {
    let _measuring = MEASURING
        .lock()
        .expect("the measuring lock is never poisoned");
    let before = ALLOCATIONS.load(Ordering::Relaxed);

    COUNTING.set(true);
    measure();
    COUNTING.set(false);

    ALLOCATIONS.load(Ordering::Relaxed) - before
}

fn i18n() -> I18n {
    I18n::builder()
        .locale_yaml(LanguageCode::En, EN)
        .expect("the locale is valid")
        .build()
        .expect("English is loaded")
}

/// Translates `key` [`CALLS`] times on a freshly built translator and returns how
/// many of those calls allocated.
fn cost_of_translating(key: &str) -> usize {
    let i18n = i18n();

    allocations_during(|| {
        for _ in 0..CALLS {
            black_box(i18n.translate(LanguageCode::En, key));
        }
    })
}

/// Translates `key` once, on a translator that is then discarded, so whatever the
/// first translation of a message does once per process is already done.
///
/// The throwaway instance matters: warming the measured translator instead would
/// fill its render cache, and a deleted fast path would then answer the whole
/// window from that cache and look just as cheap.
fn warm_up(key: &str) {
    let i18n = i18n();
    black_box(i18n.translate(LanguageCode::En, key));
}

#[test]
fn a_static_message_longer_than_the_inline_capacity_allocates_nothing() {
    warm_up("credential");
    let allocations = cost_of_translating("credential");
    let i18n = i18n();

    assert_eq!(
        i18n.translate(LanguageCode::En, "credential").as_ref(),
        LONG_MESSAGE,
        "the measured message is the long one"
    );
    assert_eq!(
        allocations, 0,
        "{CALLS} translations of a static message longer than 23 bytes must not allocate, \
         but they allocated {allocations} times"
    );
}

#[test]
fn a_static_message_within_the_inline_capacity_allocates_nothing() {
    warm_up("menu.market");
    let allocations = cost_of_translating("menu.market");
    let i18n = i18n();

    assert_eq!(
        i18n.translate(LanguageCode::En, "menu.market").as_ref(),
        "Market",
        "the measured message fits in the inline buffer"
    );
    assert_eq!(
        allocations, 0,
        "{CALLS} translations of a static message of at most 23 bytes must not allocate, \
         but they allocated {allocations} times"
    );
}

#[test]
fn the_counter_observes_the_allocations_of_an_interpolated_message() {
    let i18n = i18n();
    let last = DISTINCT_VALUES - 1;

    // The control direction: rendering an interpolated message, which the static
    // fast path bypasses, has to allocate. Every value is a fresh cache miss, so
    // this proves the counter is live instead of always reading zero.
    let rendered = allocations_during(|| {
        for value in 0..DISTINCT_VALUES {
            black_box(i18n.translate_with(LanguageCode::En, "price", &[("amount", &value)]));
        }
    });

    assert!(
        rendered > 0,
        "{DISTINCT_VALUES} distinct argument values are {DISTINCT_VALUES} cache misses, so they \
         must allocate, but the counter observed {rendered}"
    );
    assert_eq!(
        i18n.translate_with(LanguageCode::En, "price", &[("amount", &last)])
            .as_ref(),
        format!("{last} USD"),
        "the control message renders its argument"
    );
}
