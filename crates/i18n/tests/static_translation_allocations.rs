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

const CALLS: usize = 1000;

const DISTINCT_VALUES: i32 = 10;

const LONG_MESSAGE: &str = "The stored sign-in credential is invalid";

const EN: &str = r#"
menu:
  market: Market
  portfolio: Portfolio
credential: The stored sign-in credential is invalid
notice: The stored sign-in credential expired yesterday
price: "%{amount} USD"
"#;

struct CountingAllocator;

static ALLOCATIONS: AtomicUsize = AtomicUsize::new(0);

static MEASURING: Mutex<()> = Mutex::new(());

thread_local! {
    static COUNTING: Cell<bool> = const { Cell::new(false) };
}

unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        if COUNTING.with(Cell::get) {
            ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
        }

        unsafe { System.alloc(layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        if COUNTING.with(Cell::get) {
            ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
        }

        unsafe { System.realloc(ptr, layout, new_size) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) }
    }
}

#[global_allocator]
static ALLOCATOR: CountingAllocator = CountingAllocator;

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

fn cost_of_translating(key: &str) -> usize {
    let i18n = i18n();

    allocations_during(|| {
        for _ in 0..CALLS {
            black_box(i18n.translate(LanguageCode::En, key));
        }
    })
}

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
        i18n.translate(LanguageCode::En, "credential").as_str(),
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
        i18n.translate(LanguageCode::En, "menu.market").as_str(),
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
            .as_str(),
        format!("{last} USD"),
        "the control message renders its argument"
    );
}
