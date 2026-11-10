//! The fixtures and helpers the settings page's tests share.
//!
//! A test opens a page over a window the way the main window does, writes through the page's
//! own controls, and reads what the settings manager holds; this is the one place those are
//! built, so every test of the page is a test of the same arrangement.

use std::{fs, path::PathBuf};

use app_config::{
    market_participants::{
        credentials::{BinanceCredentials, OkxCredentials},
        settings::VenueSettings,
    },
    network::{ProxyAuthorizationConfig, ProxyConfig},
    settings::{AccountConfig, AccountWalletKind, Config, Language},
    translation::TranslationModel,
    ui::SidebarMode,
    workspace::{WorkspaceConfig, WorkspaceLayout, WorkspacesConfig},
};
use app_config_provider::{
    events::{AppearanceChanged, LanguageChanged},
    manager::ConfigManager,
};
use base_ui::{
    animation::ANIMATION_TICK,
    layer::Layer,
    style::{palette::Appearance as ThemeAppearance, theme::ThemeExt as _},
};
use gpui_kit::{
    AppContext as _, BorrowAppContext as _, Context, Entity, IntoElement, Modifiers,
    ParentElement as _, Render, SharedString, Styled, Subscription, TestAppContext,
    VisualTestContext, Window, div,
};
use i18n::language::codes::LanguageCode;
use market_participants_connector::connector::MarketParticipantsConnector;
use market_participants_model::identifier::VenueId;
use smol_str::SmolStr;
use tempfile::TempDir;
use widget_tokio_runtime::global;

use crate::{
    access::FieldValue,
    appearance,
    catalog::{Catalog, FieldAccess, PageDef, VenueView, catalog},
    session::SettingsSession,
    settings::{SettingsPage, SidebarEntry},
    table::ProxyRequest,
};

/// The venues every test of this module draws the settings from.
///
/// The page draws its venue pages from whichever venue list it is handed, which in production
/// is what this build speaks — see [`SettingsPage::load`] — and here is this fixture.
pub(crate) fn venues() -> VenueView {
    crate::schema::tests::venues()
}

/// The catalog of one venue list, over the fixture settings.
///
/// The catalog of the venues this build speaks.
pub(crate) fn test_catalog() -> Catalog {
    catalog(&venues())
}

/// The pages of the fixture catalog.
pub(crate) fn pages() -> Vec<PageDef> {
    test_catalog().pages
}

/// The label one page of the fixture catalog is drawn with.
///
/// A page is named by the message key it declares or by the identity it stands
/// for, and the sidebar draws whichever the catalog carries.
pub(crate) fn page_title(page: &str, cx: &TestAppContext) -> SharedString {
    cx.read(|cx| {
        test_catalog()
            .page(page)
            .unwrap_or_else(|| panic!("{page} should be in the catalog"))
            .title
            .label(cx)
    })
}

/// The window root that hosts one settings page, standing in for the main window's page
/// area.
///
/// It is also the test fixture's half of the application's projections: the language the
/// interface is rendered through and the appearance the window is drawn in. Without them a
/// preference would reach the settings but not the page drawing it.
///
/// The page is not closed from within it: the settings page has no confirm or cancel, and
/// leaving it is the main window's `show_workspace`, which the settings tests exercise by
/// asking the page for what it still holds — see [`SettingsPage::commit_pending`].
pub(crate) struct PageHost {
    page: Entity<SettingsPage>,
    _language: Subscription,
    _appearance: Subscription,
}

impl PageHost {
    fn new(page: Entity<SettingsPage>, window: &Window, cx: &mut Context<Self>) -> Self {
        let events = cx.global::<ConfigManager>().events().clone();
        let language = cx.subscribe(&events, |_, _, event: &LanguageChanged, cx| {
            cx.set_global(match event.0 {
                Language::ZhCn => LanguageCode::ZhCn,
                Language::En => LanguageCode::En,
            });
            cx.refresh_windows();
        });
        let appearance = cx.subscribe_in(
            &events,
            window,
            |_, _, event: &AppearanceChanged, window, cx| appearance::apply(event.0, window, cx),
        );

        Self {
            page,
            _language: language,
            _appearance: appearance,
        }
    }
}

impl Render for PageHost {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div().size_full().child(self.page.clone())
    }
}

/// Mounts one settings page in a fresh window and applies the stored appearance.
pub(crate) fn mount_page(
    cx: &mut TestAppContext,
    page: Entity<SettingsPage>,
) -> &mut VisualTestContext {
    let (_, cx) = cx.add_window_view(move |window, cx| {
        let host = cx.new(move |cx| PageHost::new(page, window, cx));
        Layer::new(host)
    });
    // The appearance the settings name is applied the way a window applies it when it is
    // built, so a test starts from the theme the stored preference resolves to rather than
    // from whichever one `install` left behind.
    cx.update(|window, cx| {
        let preference = cx.global::<ConfigManager>().read().appearance;
        appearance::apply(preference, window, cx);
    });
    settle(cx);

    cx
}

/// Opens the settings over a fresh window, the way the system button opens them.
///
/// What is drawn in the window is read off the window rather than off the page, so the
/// window comes back with it. The interface and the settings are both in `language`, which
/// is what a start with an interface that agrees with the settings looks like; a test that
/// writes another language is then writing a change.
pub(crate) fn settings_in(
    config: Config,
    language: Language,
    cx: &mut TestAppContext,
) -> (Entity<SettingsPage>, &mut VisualTestContext) {
    let mut config = config;
    config.language = language;
    let manager = cx.update(|cx| ConfigManager::in_memory(config, cx));

    settings_over(manager, language, cx)
}

/// Opens the settings over a manager the caller built, which is how a test gets settings
/// that cannot be stored.
///
/// The settings the manager holds are put in `language` as well as the interface: a start
/// with an interface that agrees with the settings is what a test writing another language
/// is then writing a change against.
pub(crate) fn settings_over(
    manager: ConfigManager,
    language: Language,
    cx: &mut TestAppContext,
) -> (Entity<SettingsPage>, &mut VisualTestContext) {
    settings_over_catalog(manager, language, test_catalog(), cx)
}

/// Installs the process-wide download manager a settings session reads, once per test app.
///
/// A test that opens two settings sessions in one application context installs it with the
/// first session; installing it again would replace the global and lose the downloads it
/// holds, which is exactly what the production composition root never does.
pub(crate) fn install_translation_downloads(cx: &mut gpui_kit::App) {
    install_translation_downloads_with(cx, std::sync::Arc::new(|_: &std::path::Path| Ok(())));
}

/// Installs the process-wide download manager with the unload callback a test controls.
pub(crate) fn install_translation_downloads_with(
    cx: &mut gpui_kit::App,
    unload: std::sync::Arc<
        dyn Fn(&std::path::Path) -> Result<(), crate::translation_downloads::UnloadFailure>
            + Send
            + Sync,
    >,
) {
    if crate::translation_downloads::entity(cx).is_some() {
        return;
    }

    // The process-wide download manager routes with the network settings, so it needs the
    // settings manager. A test that installs the downloads before it sets the settings up
    // gets the defaults, which the session replaces with its own manager when it starts.
    if cx.try_global::<ConfigManager>().is_none() {
        let manager = ConfigManager::in_memory(Config::default(), cx);
        cx.set_global(manager);
    }
    let provider = download_provider::global::install(
        cx,
        download_provider::global::DownloadProviderDeps {
            transport: std::sync::Arc::new(network::transport::hub::TransportHub::new(
                network::transport::hub::TransportPolicy::default(),
            )),
            runtime: widget_tokio_runtime::runtime::Runtime::for_test(),
        },
    );
    crate::translation_downloads::install(
        cx,
        crate::translation_downloads::TranslationDownloadsDeps {
            provider,
            runtime: widget_tokio_runtime::runtime::Runtime::for_test(),
            unload,
        },
    );
}

/// Opens the settings over the catalog the page is built from.
///
/// The catalog is this build's own unless a test asks otherwise: a test of what a group the
/// catalog closes is drawn as hands a catalog that closes it here.
pub(crate) fn settings_over_catalog(
    manager: ConfigManager,
    language: Language,
    catalog: Catalog,
    cx: &mut TestAppContext,
) -> (Entity<SettingsPage>, &mut VisualTestContext) {
    let page = cx.update(|cx| {
        // What a window installs before it draws anything, in the order the application
        // installs it: a field that is typed into reads the state base_ui's own fields are
        // built on.
        gpui_kit::base::init(cx);
        base_ui::style::theme::install(cx);
        i18n::global::embedded::install(cx).expect("embedded locales should load");
        cx.set_global(match language {
            Language::ZhCn => LanguageCode::ZhCn,
            Language::En => LanguageCode::En,
        });
        cx.set_global(manager);
        install_translation_downloads(cx);
        // A manager whose settings are stored under another language than the one the
        // interface is drawn in is a session that starts with a change nobody made, so the
        // two are made to agree here rather than left to what a default happens to be.
        cx.update_global::<ConfigManager, _>(|manager, cx| {
            manager.update(|config| config.language = language, cx);
        });
        let session = cx.new(|_| SettingsSession::new());

        cx.new(|cx| SettingsPage::new_with_catalog(session, catalog, cx))
    });
    let cx = mount_page(cx, page.clone());

    (page, cx)
}

/// Opens the settings with the interface in `language`.
pub(crate) fn settings(
    config: Config,
    cx: &mut TestAppContext,
) -> (Entity<SettingsPage>, &mut VisualTestContext) {
    settings_in(config, Language::En, cx)
}

/// Opens the settings over a build that has loaded nothing, and hands back the page.
///
/// The market participants root is the application's own, and the scopes the page is handed
/// are what the root reads off it: installing the root loads no venue, so the settings are
/// opened over an empty loaded-venues view and every row they offer is the static venue
/// list's.
pub(crate) fn settings_of_an_unconnected_build(
    cx: &mut TestAppContext,
) -> (Entity<SettingsPage>, &mut VisualTestContext) {
    cx.update(|cx| {
        gpui_kit::base::init(cx);
        base_ui::style::theme::install(cx);
        i18n::global::embedded::install(cx).expect("embedded locales should load");
        let manager = ConfigManager::in_memory(Config::default(), cx);
        cx.set_global(manager);
        install_translation_downloads(cx);
        global::install(cx).expect("the test Tokio runtime should build");
        MarketParticipantsConnector::install(cx)
            .expect("the market participants root should install");
    });
    let page = cx.update(|cx| cx.new(SettingsPage::load));
    let cx = mount_page(cx, page.clone());

    (page, cx)
}

/// Lets the theme's transitions and the forms' and alerts' own enter or exit transitions
/// finish and repaints.
pub(crate) fn settle(cx: &mut VisualTestContext) {
    cx.executor()
        .advance_clock(ANIMATION_TICK + std::time::Duration::from_millis(50));
    cx.update(|window, _| window.refresh());
    cx.run_until_parked();
}

/// Clicks what a selector draws, and lets whatever it opened settle.
pub(crate) fn click(cx: &mut VisualTestContext, selector: &'static str) {
    let target = cx
        .debug_bounds(selector)
        .unwrap_or_else(|| panic!("the window should draw {selector}"))
        .center();
    cx.simulate_click(target, Modifiers::none());
    settle(cx);
}

/// The access of a field of the catalog, so a test writes the way a control writes.
pub(crate) fn field_access(page: &str, field: &str) -> FieldAccess {
    test_catalog()
        .page(page)
        .unwrap_or_else(|| panic!("{page} should be in the catalog"))
        .sections
        .iter()
        .flat_map(|section| &section.fields)
        .find(|definition| definition.id == field)
        .unwrap_or_else(|| panic!("{page} should offer {field}"))
        .access
        .clone()
}

/// Writes a field the way the control of that field does: at once, into the settings.
pub(crate) fn write_field(
    panel: &Entity<SettingsPage>,
    page: &'static str,
    field: &'static str,
    value: &str,
    cx: &mut VisualTestContext,
) {
    let access = field_access(page, field);
    panel.update(cx, |panel, cx| {
        panel.write_field(access, FieldValue::new(value), cx);
    });
    settle(cx);
}

/// Moves the page the way a sidebar row does.
pub(crate) fn select(panel: &Entity<SettingsPage>, page: &str, cx: &mut VisualTestContext) {
    cx.update(|window, cx| {
        panel.update(cx, |panel, cx| panel.select(page, window, cx));
    });
    settle(cx);
}

/// Moves the caret out of whatever holds it, and draws the frame that delivers the focus
/// change — the public test API for taking the caret away from a field.
///
/// The window has to be active for GPUI to report focus changes: an inactive one blanks the
/// focus paths it hands its listeners, so the blur would never reach the field.
pub(crate) fn blur(cx: &mut VisualTestContext) {
    cx.update(|window, _| window.activate_window());
    cx.run_until_parked();
    cx.update(|window, cx| window.blur(cx));
    cx.update(|window, cx| {
        window.draw(cx).clear(cx);
    });
    settle(cx);
}

/// The settings as everyone outside this page sees them.
pub(crate) fn stored(cx: &VisualTestContext) -> Config {
    cx.read(|cx| cx.global::<ConfigManager>().read().as_ref().clone())
}

/// The process-wide downloads the harness installed.
pub(crate) fn translation_downloads(
    cx: &VisualTestContext,
) -> Entity<crate::translation_downloads::TranslationDownloads> {
    cx.read(|cx| {
        crate::translation_downloads::entity(cx)
            .expect("the settings harness installs the translation downloads")
    })
}

/// Waits for a blocking delete to finish without assuming the executor's timing.
pub(crate) fn wait_until_absent(
    cx: &mut VisualTestContext,
    downloads: &Entity<crate::translation_downloads::TranslationDownloads>,
    model: TranslationModel,
    directory: &std::path::Path,
    path: &std::path::Path,
) {
    for _ in 0..200 {
        // The delete reports through a polling GPUI task; move its timer on each turn.
        cx.executor()
            .advance_clock(std::time::Duration::from_millis(20));
        cx.run_until_parked();
        if !path.exists()
            && cx.read(|cx| downloads.read(cx).row_state(model, directory))
                == crate::translation_downloads::DownloadState::Idle
        {
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    panic!("the delete should finish: {path:?} still exists");
}

/// The appearance the application is drawn in.
pub(crate) fn drawn_appearance(cx: &VisualTestContext) -> ThemeAppearance {
    cx.read(|cx| cx.theme().appearance)
}

/// The page the dialog is drawing.
pub(crate) fn current_page(panel: &Entity<SettingsPage>, cx: &VisualTestContext) -> SharedString {
    cx.read(|cx| panel.read_with(cx, |dialog, _| dialog.current_page.clone()))
}

/// The labels the sidebar draws, in the order it draws them.
pub(crate) fn labels(panel: &Entity<SettingsPage>, cx: &VisualTestContext) -> Vec<SharedString> {
    cx.read(|cx| {
        panel.read_with(cx, |dialog, _| {
            dialog
                .sidebar
                .iter()
                .filter_map(|entry| match entry {
                    SidebarEntry::Heading(label) => Some(label.clone()),
                    SidebarEntry::Page { label, .. } => Some(label.clone()),
                    SidebarEntry::Separator => None,
                })
                .collect()
        })
    })
}

/// Settings with something written everywhere the settings can hold something, so that a
/// commit overwriting them wholesale rather than replaying changes would be visible.
pub(crate) fn configured_settings() -> Config {
    let mut config = Config::default();
    config.account = Some(AccountConfig {
        user_id: "11111111-1111-4111-8111-111111111111".into(),
        wallet_address: "0x0123456789abcdef".into(),
        wallet_kind: AccountWalletKind::Ethereum,
    });
    config.avatar = Some("<svg>configured-avatar</svg>".to_owned());
    config.proxies.insert(
        SmolStr::new_static("hk"),
        ProxyConfig {
            addr: SmolStr::new_static("127.0.0.1"),
            port: 1080,
            authorization: ProxyAuthorizationConfig::UsernameAndPassword {
                username: SmolStr::new_static("proxy-user"),
                password: SmolStr::new_static("proxy-password"),
            },
            resolve_dns_at_remote: true,
        },
    );

    {
        let VenueSettings::Binance(binance) = config
            .venues
            .get_mut(&VenueId::Binance)
            .expect("the default settings name every venue this build speaks")
        else {
            panic!("binance is configured as Binance");
        };
        binance.credentials = BinanceCredentials {
            spot_api_key: SmolStr::new_static("binance-spot-key"),
            spot_private_key: SmolStr::new_static("binance-spot-private"),
            spot_passphrase: SmolStr::new_static("binance-spot-passphrase"),
            usd_m_perp_api_key: SmolStr::new_static("binance-perp-key"),
            usd_m_perp_private_key: SmolStr::new_static("binance-perp-private"),
            usd_m_perp_passphrase: SmolStr::new_static("binance-perp-passphrase"),
        };
    }

    {
        let VenueSettings::Okx(okx) = config
            .venues
            .get_mut(&VenueId::Okx)
            .expect("the default settings name every venue this build speaks")
        else {
            panic!("okx is configured as OKX");
        };
        okx.credentials = OkxCredentials {
            api_key: SmolStr::new_static("okx-api-key"),
            secret_key: SmolStr::new_static("okx-secret-key"),
            passphrase: SmolStr::new_static("okx-passphrase"),
        };
    }

    config
}

/// A manager whose settings cannot be stored, the file that blocks it, and the directory the
/// two of them are in.
///
/// The settings sit behind a path that is a file rather than a directory, so creating the
/// config directory there fails the way an unwritable config directory does. The settings it
/// starts with are the defaults, which is what a first start looks like and what a language
/// of `en` — the one the interface of a test starts in — is a change of.
///
/// The path is one this run has to itself: it is made inside a directory that is created for
/// the test and removed when the returned handle is dropped, so the file that blocks the
/// settings is written at a path nothing else can already hold. A name built from the process
/// id and a counter is not — a later run of a recycled id reaches the same name, and writing
/// a file where that run left a directory fails with access denied instead of with the
/// failure a test is about.
///
/// A test that repairs the path the way a user repairs a permission is the one thing that can
/// tell a retry apart from a commit that was never attempted again, so the handle lives for as
/// long as the path is used: dropping it removes the directory and what the test put in it.
pub(crate) fn manager_behind_a_file(cx: &mut TestAppContext) -> (ConfigManager, PathBuf, TempDir) {
    let directory = test_temp("manager-behind-a-file");
    let blocking = directory.path().join("not-a-directory");
    fs::write(&blocking, b"block directory creation").expect("the blocking file is written");

    let manager = cx
        .update(|cx| ConfigManager::from_path(blocking.join("config.json"), cx))
        .expect("a config manager loads around a missing file");

    (manager, blocking, directory)
}

/// A temporary directory of the test's own, under the workspace's build output.
///
/// The system temporary directory is not usable here: a test that runs behind a restricted
/// integrity token cannot create directories in `%TEMP%`, which leaves every test about a file
/// on disk failing for a reason that has nothing to do with the code under test. The build
/// output is the workspace's own writable scratch space, so the directory goes there.
pub(crate) fn test_temp(name: &str) -> TempDir {
    let workspace = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(std::path::Path::parent)
        .expect("this crate lives two directories below the workspace root");
    let root = workspace.join("target").join("test-temp").join("settings");
    fs::create_dir_all(&root).expect("the test temporary directory should be created");
    tempfile::Builder::new()
        .prefix(name)
        .tempdir_in(&root)
        .expect("a temporary directory should be created")
}

/// What the rest of the application writes while the settings are open: an avatar the
/// background generated, an account it signed in, a layout it persisted, a sidebar state
/// the main window saved.
///
/// Every one of them goes through `update`, which is the path those subsystems own; the
/// dialog must neither know about them nor undo them.
pub(crate) fn write_while_open(cx: &mut VisualTestContext) {
    cx.update(|_, cx| {
        cx.update_global::<ConfigManager, _>(|manager, cx| {
            manager.update(
                |config| {
                    config.avatar = Some("<svg>generated-while-open</svg>".to_owned());
                    config.account = Some(AccountConfig {
                        user_id: "22222222-2222-4222-8222-222222222222".to_owned(),
                        wallet_address: "0xfeed".to_owned(),
                        wallet_kind: AccountWalletKind::Ethereum,
                    });
                    config.workspaces = WorkspacesConfig {
                        entries: vec![WorkspaceConfig {
                            name: SmolStr::new_static("written-while-open"),
                            layout: WorkspaceLayout::single_pane(),
                        }],
                        active: Some(SmolStr::new_static("written-while-open")),
                    };
                    config.ui.sidebar = SidebarMode::IconOnly;
                },
                cx,
            );
        });
    });
}

/// Opens the form that edits an entry, the way the row of the catalog that asks for one
/// does.
pub(crate) fn open_edit_form(
    panel: &Entity<SettingsPage>,
    name: &'static str,
    cx: &mut VisualTestContext,
) {
    request(
        panel,
        ProxyRequest::Edit {
            name: SmolStr::new_static(name),
        },
        cx,
    );
}

/// Asks the catalog for something a row asks for, and lets the dialog take it.
///
/// The window is not refreshed here, which is what makes the wait worth anything: the request
/// is what marks the window dirty, and the frame that follows — drawn inside the effect flush —
/// is what the assertions after this rely on, so refreshing here would draw what the request
/// opened even if the request alone had left it undrawn. What the clock is for is the opened
/// form's own transition, which the next `settle` or `click` moves on.
pub(crate) fn request(
    panel: &Entity<SettingsPage>,
    request: ProxyRequest,
    cx: &mut VisualTestContext,
) {
    panel.update(cx, |dialog, cx| {
        dialog.proxies.update(cx, |table, cx| {
            table.request(request, cx);
        });
    });
    cx.executor()
        .advance_clock(ANIMATION_TICK + std::time::Duration::from_millis(50));
    cx.run_until_parked();
}

/// Opens the form that adds an entry, the way the button above the catalog asks for one.
pub(crate) fn open_add_form(panel: &Entity<SettingsPage>, cx: &mut VisualTestContext) {
    request(panel, ProxyRequest::Add, cx);
}

/// Clicks one text field of a form and types into it without leaving it.
///
/// The caret is left where it is, which is what a user who types and then presses a button
/// does: it is the case that tells a field committed on blur from one the form reads.
pub(crate) fn type_into(cx: &mut VisualTestContext, selector: &'static str, text: &str) {
    let field = cx
        .debug_bounds(selector)
        .unwrap_or_else(|| panic!("the form should draw {selector}"));
    cx.simulate_click(field.center(), Modifiers::none());
    cx.simulate_input(text);
    settle(cx);
}

/// Asks the catalog for a delete, the way a row of it asks for one, and lets the dialog take
/// it.
///
/// What a row asks for is the alert rather than the removal: the entry leaves the session
/// through the alert's own action, so a test that wants it gone confirms it there.
pub(crate) fn request_delete(
    panel: &Entity<SettingsPage>,
    name: &'static str,
    cx: &mut VisualTestContext,
) {
    request(
        panel,
        ProxyRequest::Delete {
            name: SmolStr::new_static(name),
        },
        cx,
    );
}
