#![allow(unsafe_op_in_unsafe_fn)]

use anyhow::{Context, Result};
use keyring::Entry;
use objc2::rc::Retained;
use objc2::runtime::{AnyObject, Bool, Sel};
use objc2::{MainThreadMarker, class, msg_send, sel};
use objc2_app_kit::{NSButton, NSColor};
use objc2_core_graphics::CGColor;
use objc2_foundation::{NSPoint, NSRect, NSSize, NSString};
use once_cell::sync::OnceCell;
use std::sync::{
    Mutex,
    atomic::{AtomicUsize, Ordering},
};

use crate::macos::{
    NIL, ObjcId, app, cache_snapshot, cached_snapshot, clear_snapshot_cache, ns_string,
    ns_string_to_rust, show_error_alert,
};
use crate::vercel::{AppSnapshot, VercelClient};

pub(crate) const VERCEL_TOKEN_SETTINGS_URL: &str = "https://vercel.com/account/settings/tokens";
const KEYRING_SERVICE: &str = "dev-tree";
const KEYRING_ACCOUNT: &str = "vercel-token";
const TOKEN_DEFAULTS_KEY: &str = "vercelToken";
const SETTINGS_SAVE: i64 = 1;
const SETTINGS_CLEAR: i64 = 2;
const PROJECT_SORT_KEY: &str = "projectSortOrder";
const AUTO_REFRESH_KEY: &str = "autoRefreshMinutes";
const VISIBILITY_KEY_PREFIX: &str = "visibility";
const SORT_LAST_DEPLOYMENT: i64 = 0;
pub(crate) const SORT_ALPHABETICAL: i64 = 1;
const AUTO_REFRESH_OPTIONS: [u64; 5] = [1, 5, 15, 30, 60];

static SETTINGS_WINDOW: AtomicUsize = AtomicUsize::new(0);
static SETTINGS_FIELD: AtomicUsize = AtomicUsize::new(0);
static SETTINGS_SORT: AtomicUsize = AtomicUsize::new(0);
static SETTINGS_REFRESH: AtomicUsize = AtomicUsize::new(0);
static SETTINGS_RESPONSE: AtomicUsize = AtomicUsize::new(0);
struct VisibilityControl {
    key: String,
    button: usize,
    team_name: String,
    is_team: bool,
}

static SETTINGS_VISIBILITY: OnceCell<Mutex<Vec<VisibilityControl>>> = OnceCell::new();
static SETTINGS_CONTENT: AtomicUsize = AtomicUsize::new(0);
static SETTINGS_TREE: AtomicUsize = AtomicUsize::new(0);
static SETTINGS_SNAPSHOT: OnceCell<Mutex<Option<AppSnapshot>>> = OnceCell::new();
static SETTINGS_PENDING_VISIBILITY: OnceCell<Mutex<Vec<(String, i64)>>> = OnceCell::new();
static SETTINGS_ORIGINAL_TOKEN: OnceCell<Mutex<Option<String>>> = OnceCell::new();

macro_rules! send_void {
    ($receiver:expr, $($message:tt)*) => {{
        let _: () = msg_send![$receiver, $($message)*];
    }};
}

unsafe fn add_settings_symbol(content: ObjcId, symbol: &str, description: &str, y: f64) {
    let image: ObjcId = msg_send![
        class!(NSImage),
        imageWithSystemSymbolName: ns_string(symbol),
        accessibilityDescription: ns_string(description),
    ];
    if image == NIL {
        return;
    }
    let image_view: ObjcId = msg_send![class!(NSImageView), alloc];
    let image_view: ObjcId = msg_send![
        image_view,
        initWithFrame: NSRect::new(
            NSPoint::new(24.0, y + 2.0),
            NSSize::new(20.0, 20.0)
        )
    ];
    send_void![image_view, setImage: image];
    send_void![image_view, setImageScaling: 3i64];
    send_void![image_view, setImageAlignment: 0i64];
    let tint: ObjcId = msg_send![class!(NSColor), secondaryLabelColor];
    send_void![image_view, setContentTintColor: tint];
    send_void![content, addSubview: image_view];
}

unsafe fn settings_heading_font() -> ObjcId {
    let rounded: ObjcId = msg_send![
        class!(NSFont),
        fontWithName: ns_string("SF Pro Rounded Semibold"),
        size: 18.0f64,
    ];
    if rounded != NIL {
        rounded
    } else {
        msg_send![
            class!(NSFont),
            systemFontOfSize: 18.0f64,
            weight: 0.4f64,
        ]
    }
}

pub(crate) fn visibility_key(kind: &str, name: &str) -> String {
    format!("{VISIBILITY_KEY_PREFIX}.{kind}.{name}")
}

fn visibility_defaults() -> &'static Mutex<Vec<VisibilityControl>> {
    SETTINGS_VISIBILITY.get_or_init(|| Mutex::new(Vec::new()))
}

pub(crate) unsafe fn is_visible(key: &str) -> bool {
    let defaults: ObjcId = msg_send![class!(NSUserDefaults), standardUserDefaults];
    let value: ObjcId = msg_send![defaults, objectForKey: ns_string(key)];
    let stored: bool = msg_send![defaults, boolForKey: ns_string(key)];
    value == NIL || stored
}

fn pending_visibility() -> &'static Mutex<Vec<(String, i64)>> {
    SETTINGS_PENDING_VISIBILITY.get_or_init(|| Mutex::new(Vec::new()))
}

unsafe fn add_visibility_checkbox(
    parent: ObjcId,
    title: &str,
    key: String,
    team_name: &str,
    is_team: bool,
    controller: ObjcId,
    origin: NSPoint,
) -> ObjcId {
    let mtm = MainThreadMarker::new().expect("AppKit must run on the main thread");
    let button: ObjcId = Retained::into_raw(NSButton::buttonWithTitle_target_action(
        &NSString::from_str(title),
        Some(&*(controller as *const AnyObject)),
        Some(sel!(settingsVisibilityChanged:)),
        mtm,
    ))
    .cast();
    send_void![button, setButtonType: 3i64];
    let state = pending_visibility()
        .lock()
        .unwrap()
        .iter()
        .find(|(pending_key, _)| pending_key == &key)
        .map(|(_, state)| *state)
        .unwrap_or_else(|| if is_visible(&key) { 1i64 } else { 0i64 });
    if is_team {
        send_void![button, setAllowsMixedState: Bool::YES];
    }
    send_void![button, setState: state];
    send_void![button, setFrame: NSRect::new(
        origin,
        NSSize::new(380.0, 24.0)
    )];
    send_void![parent, addSubview: button];
    visibility_defaults()
        .lock()
        .unwrap()
        .push(VisibilityControl {
            key,
            button: button as usize,
            team_name: team_name.to_owned(),
            is_team,
        });
    button
}

unsafe fn add_visibility_tree(
    content: ObjcId,
    snapshot: Option<&AppSnapshot>,
    sort_order: i64,
    controller: ObjcId,
) -> ObjcId {
    let document_height = snapshot
        .map(|snapshot| {
            snapshot
                .teams
                .iter()
                .map(|team| 1 + team.projects.len())
                .sum::<usize>()
                .max(1) as f64
                * 26.0
                + 8.0
        })
        .unwrap_or(40.0);
    let document: ObjcId = msg_send![class!(NSView), alloc];
    let document: ObjcId = msg_send![
        document,
        initWithFrame: NSRect::new(
            NSPoint::new(0.0, 0.0),
            NSSize::new(404.0, document_height)
        )
    ];
    if let Some(snapshot) = snapshot {
        let mut y = document_height - 32.0;
        for team in &snapshot.teams {
            add_visibility_checkbox(
                document,
                &team.name,
                visibility_key("team", &team.name),
                &team.name,
                true,
                controller,
                NSPoint::new(8.0, y),
            );
            y -= 26.0;
            let mut projects = team.projects.clone();
            if sort_order == SORT_ALPHABETICAL {
                projects.sort_by_key(|project| project.name.to_ascii_lowercase());
            } else {
                projects.sort_by_key(|project| std::cmp::Reverse(project.latest_deployment_at()));
            }
            for project in &projects {
                add_visibility_checkbox(
                    document,
                    &project.name,
                    visibility_key("project", &format!("{}::{}", team.name, project.name)),
                    &team.name,
                    false,
                    controller,
                    NSPoint::new(28.0, y),
                );
                y -= 26.0;
            }
            update_team_checkbox(&team.name);
        }
    } else {
        let label: ObjcId =
            msg_send![class!(NSTextField), labelWithString: ns_string("No projects loaded yet.")];
        send_void![label, setFrame: NSRect::new(
            NSPoint::new(8.0, 8.0),
            NSSize::new(380.0, 24.0)
        )];
        send_void![document, addSubview: label];
    }

    let container: ObjcId = msg_send![class!(NSVisualEffectView), alloc];
    let container: ObjcId = msg_send![
        container,
        initWithFrame: NSRect::new(
            NSPoint::new(56.0, 72.0),
            NSSize::new(420.0, 210.0)
        )
    ];
    send_void![container, setMaterial: 6i64];
    send_void![container, setBlendingMode: 0i64];
    send_void![container, setState: 1i64];
    send_void![container, setWantsLayer: Bool::YES];
    let container_layer: ObjcId = msg_send![container, layer];
    send_void![container_layer, setCornerRadius: 10.0f64];
    send_void![container_layer, setBorderWidth: 1.0f64];
    let border_color: ObjcId = msg_send![class!(NSColor), separatorColor];
    let border_cg_color: *mut CGColor =
        Retained::into_raw((&*(border_color as *const AnyObject as *const NSColor)).CGColor());
    send_void![container_layer, setBorderColor: border_cg_color];
    let tint: ObjcId = msg_send![class!(NSColor), controlAccentColor];
    let tint: ObjcId = msg_send![tint, colorWithAlphaComponent: 0.06f64];
    let tint_cg_color: *mut CGColor =
        Retained::into_raw((&*(tint as *const AnyObject as *const NSColor)).CGColor());
    send_void![container_layer, setBackgroundColor: tint_cg_color];
    send_void![content, addSubview: container];

    let scroll: ObjcId = msg_send![class!(NSScrollView), alloc];
    let scroll: ObjcId = msg_send![
        scroll,
        initWithFrame: NSRect::new(
            NSPoint::new(64.0, 80.0),
            NSSize::new(404.0, 194.0)
        )
    ];
    send_void![scroll, setHasVerticalScroller: Bool::YES];
    send_void![scroll, setBorderType: 0i64];
    send_void![scroll, setDrawsBackground: Bool::NO];
    send_void![scroll, setDocumentView: document];
    send_void![content, addSubview: scroll];
    let clip_view: ObjcId = msg_send![scroll, contentView];
    let clip_bounds: NSRect = msg_send![clip_view, bounds];
    let top_y = (document_height - clip_bounds.size.height).max(0.0);
    send_void![
        clip_view,
        scrollToPoint: NSPoint::new(0.0, top_y)
    ];
    send_void![scroll, reflectScrolledClipView: clip_view];
    pending_visibility().lock().unwrap().clear();
    scroll
}

unsafe fn update_team_checkbox(team_name: &str) {
    let (team_button, project_buttons) = {
        let controls = visibility_defaults().lock().unwrap();
        let team_button = controls
            .iter()
            .find(|control| control.is_team && control.team_name == team_name)
            .map(|control| control.button as ObjcId);
        let project_buttons = controls
            .iter()
            .filter(|control| !control.is_team && control.team_name == team_name)
            .map(|control| control.button as ObjcId)
            .collect::<Vec<_>>();
        (team_button, project_buttons)
    };
    let Some(team_button) = team_button else {
        return;
    };
    if project_buttons.is_empty() {
        return;
    }
    let selected_count = project_buttons
        .iter()
        .filter(|button| {
            let state: i64 = msg_send![**button, state];
            state == 1
        })
        .count();
    let team_state = team_checkbox_state(selected_count, project_buttons.len());
    send_void![team_button, setState: team_state];
}

fn team_checkbox_state(selected_count: usize, project_count: usize) -> i64 {
    if selected_count == 0 {
        0
    } else if selected_count == project_count {
        1
    } else {
        -1
    }
}

pub(crate) fn show_settings_modal(controller: ObjcId) -> Result<()> {
    unsafe {
        let mtm = MainThreadMarker::new().expect("AppKit must run on the main thread");
        let app = app();
        send_void![app, activateIgnoringOtherApps: Bool::YES];
        let initial_token = load_token()?;
        SETTINGS_ORIGINAL_TOKEN
            .get_or_init(|| Mutex::new(None))
            .lock()
            .unwrap()
            .clone_from(&initial_token);
        let snapshot = match cached_snapshot() {
            Some(snapshot) => Some(snapshot),
            None => match initial_token.as_deref() {
                Some(token) => {
                    let snapshot = VercelClient::new(token)?.fetch_snapshot()?;
                    cache_snapshot(snapshot.clone());
                    Some(snapshot)
                }
                None => None,
            },
        };
        let window: ObjcId = msg_send![class!(NSWindow), alloc];
        let window: ObjcId = msg_send![
            window,
            initWithContentRect: NSRect::new(
                NSPoint::new(0.0, 0.0),
                NSSize::new(500.0, 600.0),
            ),
            styleMask: (1u64 | 2u64 | 32768u64),
            backing: 2u64,
            defer: Bool::NO,
        ];
        send_void![window, setTitle: ns_string("Settings")];
        send_void![window, setTitlebarAppearsTransparent: Bool::YES];
        send_void![window, setTitleVisibility: 0i64];
        send_void![window, setReleasedWhenClosed: Bool::NO];
        send_void![window, setInitialFirstResponder: NIL];
        send_void![window, setDelegate: controller];
        let close_button: ObjcId = msg_send![window, standardWindowButton: 2i64];
        send_void![close_button, setTarget: controller];
        send_void![close_button, setAction: sel!(settingsCancel:)];
        send_void![window, center];
        let content: ObjcId = msg_send![window, contentView];

        let background: ObjcId = msg_send![class!(NSVisualEffectView), alloc];
        let background: ObjcId = msg_send![
            background,
            initWithFrame: NSRect::new(
                NSPoint::new(0.0, 0.0),
                NSSize::new(500.0, 600.0)
            )
        ];
        send_void![background, setMaterial: 7i64];
        send_void![background, setBlendingMode: 0i64];
        send_void![background, setState: 1i64];
        send_void![background, setAutoresizingMask: 18u64];
        send_void![content, addSubview: background];

        visibility_defaults().lock().unwrap().clear();
        pending_visibility().lock().unwrap().clear();
        SETTINGS_SNAPSHOT
            .get_or_init(|| Mutex::new(None))
            .lock()
            .unwrap()
            .clone_from(&snapshot);
        SETTINGS_CONTENT.store(content as usize, Ordering::Relaxed);
        add_settings_symbol(content, "person.crop.circle", "Vercel account", 531.0);
        let account_label: ObjcId =
            msg_send![class!(NSTextField), labelWithString: ns_string("Vercel Account")];
        send_void![account_label, setFrame: NSRect::new(
            NSPoint::new(56.0, 530.0), NSSize::new(300.0, 24.0)
        )];
        let heading_font = settings_heading_font();
        send_void![account_label, setFont: heading_font];
        send_void![content, addSubview: account_label];
        let label: ObjcId = msg_send![class!(NSTextField), labelWithString: ns_string(
            "Vercel token (used to load teams and projects):"
        )];
        send_void![label, setFrame: NSRect::new(
            NSPoint::new(56.0, 504.0), NSSize::new(420.0, 20.0)
        )];
        let secondary_color: ObjcId = msg_send![class!(NSColor), secondaryLabelColor];
        send_void![label, setTextColor: secondary_color];
        send_void![content, addSubview: label];

        let text_field: ObjcId = msg_send![class!(NSSecureTextField), alloc];
        let text_field: ObjcId = msg_send![text_field, initWithFrame: NSRect::new(
            NSPoint::new(56.0, 460.0), NSSize::new(420.0, 36.0)
        )];
        send_void![text_field, setEditable: Bool::YES];
        send_void![text_field, setSelectable: Bool::YES];
        if let Some(token) = initial_token {
            send_void![text_field, setStringValue: ns_string(&token)];
        }
        send_void![content, addSubview: text_field];

        let clear: ObjcId = Retained::into_raw(NSButton::buttonWithTitle_target_action(
            &NSString::from_str("Clear Token"),
            Some(&*(controller as *const AnyObject)),
            Some(sel!(settingsClear:)),
            mtm,
        ))
        .cast();
        send_void![clear, setFrame: NSRect::new(
            NSPoint::new(56.0, 420.0), NSSize::new(120.0, 28.0)
        )];
        let trash: ObjcId = msg_send![
            class!(NSImage),
            imageWithSystemSymbolName: ns_string("trash"),
            accessibilityDescription: ns_string("Delete"),
        ];
        if trash != NIL {
            send_void![clear, setImage: trash];
            send_void![clear, setImagePosition: 2i64];
            send_void![clear, setImageHugsTitle: Bool::NO];
        }
        send_void![clear, setHasDestructiveAction: Bool::YES];
        send_void![content, addSubview: clear];
        let generate: ObjcId = Retained::into_raw(NSButton::buttonWithTitle_target_action(
            &NSString::from_str("Generate Token"),
            Some(&*(controller as *const AnyObject)),
            Some(sel!(openTokenSettings:)),
            mtm,
        ))
        .cast();
        send_void![generate, setFrame: NSRect::new(
            NSPoint::new(336.0, 420.0), NSSize::new(140.0, 28.0)
        )];
        let external_link: ObjcId = msg_send![
            class!(NSImage),
            imageWithSystemSymbolName: ns_string("arrow.up.right"),
            accessibilityDescription: ns_string("Open externally"),
        ];
        if external_link != NIL {
            send_void![generate, setImage: external_link];
            send_void![generate, setImagePosition: 3i64];
            send_void![generate, setImageHugsTitle: Bool::NO];
        }
        send_void![content, addSubview: generate];

        add_settings_symbol(content, "square.grid.2x2", "Projects display", 369.0);
        let display_label: ObjcId =
            msg_send![class!(NSTextField), labelWithString: ns_string("Projects Display")];
        send_void![display_label, setFrame: NSRect::new(
            NSPoint::new(56.0, 368.0), NSSize::new(420.0, 24.0)
        )];
        send_void![display_label, setFont: heading_font];
        send_void![content, addSubview: display_label];
        let sort_label: ObjcId =
            msg_send![class!(NSTextField), labelWithString: ns_string("Projects sort order")];
        send_void![sort_label, setFrame: NSRect::new(
            NSPoint::new(56.0, 298.0), NSSize::new(160.0, 22.0)
        )];
        send_void![content, addSubview: sort_label];
        let sort_popup: ObjcId = msg_send![class!(NSPopUpButton), alloc];
        let sort_popup: ObjcId = msg_send![
            sort_popup,
            initWithFrame: NSRect::new(
                NSPoint::new(270.0, 298.0),
                NSSize::new(206.0, 28.0),
            ),
            pullsDown: Bool::NO,
        ];
        send_void![sort_popup, setTarget: controller];
        send_void![sort_popup, setAction: sel!(settingsSortChanged:)];
        send_void![sort_popup, addItemWithTitle: ns_string("Last deployment date")];
        send_void![sort_popup, addItemWithTitle: ns_string("Alphabetically")];
        send_void![sort_popup, selectItemAtIndex: project_sort_order()];
        send_void![content, addSubview: sort_popup];
        let refresh_label: ObjcId =
            msg_send![class!(NSTextField), labelWithString: ns_string("Auto-refresh")];
        send_void![refresh_label, setFrame: NSRect::new(
            NSPoint::new(56.0, 332.0), NSSize::new(160.0, 22.0)
        )];
        send_void![content, addSubview: refresh_label];
        let refresh_popup: ObjcId = msg_send![class!(NSPopUpButton), alloc];
        let refresh_popup: ObjcId = msg_send![
            refresh_popup,
            initWithFrame: NSRect::new(
                NSPoint::new(270.0, 334.0),
                NSSize::new(206.0, 28.0),
            ),
            pullsDown: Bool::NO,
        ];
        for minutes in AUTO_REFRESH_OPTIONS {
            send_void![
                refresh_popup,
                addItemWithTitle: ns_string(&format!("{minutes} minutes"))
            ];
        }
        send_void![
            refresh_popup,
            selectItemAtIndex: AUTO_REFRESH_OPTIONS
                .iter()
                .position(|option| *option == auto_refresh_minutes())
                .unwrap_or(1) as i64
        ];
        send_void![content, addSubview: refresh_popup];
        let tree =
            add_visibility_tree(content, snapshot.as_ref(), project_sort_order(), controller);
        SETTINGS_TREE.store(tree as usize, Ordering::Relaxed);

        let cancel: ObjcId = msg_send![
            class!(NSButton),
            buttonWithTitle: ns_string("Cancel"),
            target: controller,
            action: sel!(settingsCancel:),
        ];
        send_void![cancel, setFrame: NSRect::new(
            NSPoint::new(326.0, 20.0), NSSize::new(70.0, 32.0)
        )];
        let save: ObjcId = msg_send![
            class!(NSButton),
            buttonWithTitle: ns_string("Save"),
            target: controller,
            action: sel!(settingsSave:),
        ];
        send_void![save, setFrame: NSRect::new(
            NSPoint::new(406.0, 20.0), NSSize::new(70.0, 32.0)
        )];
        send_void![save, setKeyEquivalent: ns_string("\r")];
        send_void![save, setKeyEquivalentModifierMask: 0u64];
        send_void![cancel, setKeyEquivalent: ns_string("\u{1b}")];
        send_void![cancel, setKeyEquivalentModifierMask: 0u64];
        send_void![save, setBezelStyle: 1i64];
        send_void![content, addSubview: cancel];
        send_void![content, addSubview: save];
        SETTINGS_WINDOW.store(window as usize, Ordering::Relaxed);
        SETTINGS_FIELD.store(text_field as usize, Ordering::Relaxed);
        SETTINGS_SORT.store(sort_popup as usize, Ordering::Relaxed);
        SETTINGS_REFRESH.store(refresh_popup as usize, Ordering::Relaxed);
        SETTINGS_RESPONSE.store(0, Ordering::Relaxed);
        send_void![window, makeKeyAndOrderFront: NIL];
        let _: Bool = msg_send![window, makeFirstResponder: NIL];
        let _: i64 = msg_send![app, runModalForWindow: window];
        let response = SETTINGS_RESPONSE.load(Ordering::Relaxed) as i64;
        send_void![window, orderOut: NIL];
        SETTINGS_WINDOW.store(0, Ordering::Relaxed);
        SETTINGS_FIELD.store(0, Ordering::Relaxed);
        SETTINGS_SORT.store(0, Ordering::Relaxed);
        SETTINGS_REFRESH.store(0, Ordering::Relaxed);
        SETTINGS_CONTENT.store(0, Ordering::Relaxed);
        SETTINGS_TREE.store(0, Ordering::Relaxed);
        if response == SETTINGS_SAVE {
            Ok(())
        } else if response == SETTINGS_CLEAR {
            clear_token()?;
            clear_snapshot_cache();
            Ok(())
        } else {
            Ok(())
        }
    }
}

pub(crate) fn load_token() -> Result<Option<String>> {
    if let Ok(token) = std::env::var("VERCEL_TOKEN")
        && !token.trim().is_empty()
    {
        return Ok(Some(token.trim().to_owned()));
    }
    let entry = token_entry()?;
    match entry.get_password() {
        Ok(token) if token.trim().is_empty() => Ok(None),
        Ok(token) => Ok(Some(token)),
        Err(keyring::Error::NoEntry) => load_token_from_defaults(),
        Err(err) => Err(err).context("Keychain lookup failed"),
    }
}

fn save_token(token: &str) -> Result<()> {
    token_entry()?
        .set_password(token)
        .context("Failed to save token to the macOS keychain")?;
    unsafe {
        let defaults: ObjcId = msg_send![class!(NSUserDefaults), standardUserDefaults];
        send_void![
            defaults,
            setObject: ns_string(token),
            forKey: ns_string(TOKEN_DEFAULTS_KEY),
        ];
    }
    Ok(())
}

fn clear_token() -> Result<()> {
    let entry = token_entry()?;
    match entry.delete_credential() {
        Ok(()) | Err(keyring::Error::NoEntry) => {
            unsafe {
                let defaults: ObjcId = msg_send![class!(NSUserDefaults), standardUserDefaults];
                send_void![defaults, removeObjectForKey: ns_string(TOKEN_DEFAULTS_KEY)];
            }
            Ok(())
        }
        Err(err) => Err(err).context("Failed to clear saved token"),
    }
}

fn load_token_from_defaults() -> Result<Option<String>> {
    unsafe {
        let defaults: ObjcId = msg_send![class!(NSUserDefaults), standardUserDefaults];
        let value: ObjcId = msg_send![defaults, objectForKey: ns_string(TOKEN_DEFAULTS_KEY)];
        if value == NIL {
            Ok(None)
        } else {
            let token = ns_string_to_rust(value);
            if token.trim().is_empty() {
                Ok(None)
            } else {
                Ok(Some(token))
            }
        }
    }
}

fn token_entry() -> Result<Entry> {
    Entry::new(KEYRING_SERVICE, KEYRING_ACCOUNT).context("Failed to open keychain entry")
}

pub(crate) extern "C" fn settings_sort_changed(this: ObjcId, _: Sel, sender: ObjcId) {
    unsafe {
        let content = SETTINGS_CONTENT.load(Ordering::Relaxed) as ObjcId;
        let old_tree = SETTINGS_TREE.load(Ordering::Relaxed) as ObjcId;
        if content == NIL {
            return;
        }
        if old_tree != NIL {
            send_void![old_tree, removeFromSuperview];
        }
        {
            let controls = visibility_defaults().lock().unwrap();
            let mut pending = pending_visibility().lock().unwrap();
            pending.extend(controls.iter().map(|control| {
                let button = control.button as ObjcId;
                let state: i64 = msg_send![button, state];
                (control.key.clone(), state)
            }));
        }
        visibility_defaults().lock().unwrap().clear();
        let sort: i64 = msg_send![sender, indexOfSelectedItem];
        let snapshot = SETTINGS_SNAPSHOT
            .get_or_init(|| Mutex::new(None))
            .lock()
            .unwrap()
            .clone();
        let tree = add_visibility_tree(content, snapshot.as_ref(), sort, this);
        SETTINGS_TREE.store(tree as usize, Ordering::Relaxed);
    }
}

pub(crate) extern "C" fn settings_visibility_changed(_: ObjcId, _: Sel, sender: ObjcId) {
    unsafe {
        let (team_name, is_team, project_buttons) = {
            let controls = visibility_defaults().lock().unwrap();
            let Some(changed_control) = controls
                .iter()
                .find(|control| control.button == sender as usize)
            else {
                return;
            };
            let team_name = changed_control.team_name.clone();
            let is_team = changed_control.is_team;
            let project_buttons = controls
                .iter()
                .filter(|control| !control.is_team && control.team_name == team_name)
                .map(|control| control.button as ObjcId)
                .collect::<Vec<_>>();
            (team_name, is_team, project_buttons)
        };
        if is_team {
            let select_all = project_buttons.iter().any(|button| {
                let state: i64 = msg_send![*button, state];
                state != 1
            });
            let state = if select_all { 1i64 } else { 0i64 };
            for button in project_buttons {
                send_void![button, setState: state];
            }
        }
        update_team_checkbox(&team_name);
    }
}

pub(crate) extern "C" fn settings_save(_: ObjcId, _: Sel, _: ObjcId) {
    unsafe {
        let field = SETTINGS_FIELD.load(Ordering::Relaxed) as ObjcId;
        let token = normalize_token(&ns_string_to_rust(msg_send![field, stringValue]));
        let original_token = SETTINGS_ORIGINAL_TOKEN
            .get_or_init(|| Mutex::new(None))
            .lock()
            .unwrap()
            .clone();
        let new_token = (!token.is_empty()).then_some(token.as_str());
        let token_changed = original_token.as_deref() != new_token;
        let result = if token.is_empty() && token_changed {
            clear_token()
        } else if token.is_empty() || original_token.as_deref() == Some(token.as_str()) {
            Ok(())
        } else {
            VercelClient::new(&token)
                .and_then(|client| client.validate_token())
                .context("The provided Vercel token is invalid")
                .and_then(|_| save_token(&token))
        };
        match result {
            Ok(()) => {
                let sort_popup = SETTINGS_SORT.load(Ordering::Relaxed) as ObjcId;
                let sort: i64 = msg_send![sort_popup, indexOfSelectedItem];
                set_project_sort_order(sort);
                let refresh_popup = SETTINGS_REFRESH.load(Ordering::Relaxed) as ObjcId;
                let refresh_index: i64 = msg_send![refresh_popup, indexOfSelectedItem];
                set_auto_refresh_minutes(
                    AUTO_REFRESH_OPTIONS
                        .get(refresh_index as usize)
                        .copied()
                        .unwrap_or(5),
                );
                let defaults: ObjcId = msg_send![class!(NSUserDefaults), standardUserDefaults];
                for control in visibility_defaults().lock().unwrap().iter() {
                    let button = control.button as ObjcId;
                    let state: i64 = msg_send![button, state];
                    send_void![
                        defaults,
                        setBool: state != 0,
                        forKey: ns_string(&control.key),
                    ];
                }
                let _: Bool = msg_send![defaults, synchronize];
                if token_changed {
                    clear_snapshot_cache();
                }
                SETTINGS_RESPONSE.store(SETTINGS_SAVE as usize, Ordering::Relaxed);
                send_void![app(), stopModal];
            }

            Err(err) => show_error_alert("Failed to update Vercel token", &err.to_string()),
        }
    }
}

fn normalize_token(value: &str) -> String {
    let value = value.trim();
    let value = value.strip_prefix('#').unwrap_or(value).trim();
    let value = value
        .strip_prefix("VERCEL_TOKEN=")
        .unwrap_or(value)
        .trim()
        .trim_matches('"')
        .trim_matches('\'');
    value.split_whitespace().collect()
}

pub(crate) extern "C" fn settings_cancel(_: ObjcId, _: Sel, _: ObjcId) {
    SETTINGS_RESPONSE.store(0, Ordering::Relaxed);
    unsafe {
        send_void![app(), stopModal];
    }
}

pub(crate) extern "C" fn settings_clear(_: ObjcId, _: Sel, _: ObjcId) {
    SETTINGS_RESPONSE.store(SETTINGS_CLEAR as usize, Ordering::Relaxed);
    unsafe {
        send_void![app(), stopModal];
    }
}

pub(crate) fn project_sort_order() -> i64 {
    unsafe {
        let defaults: ObjcId = msg_send![class!(NSUserDefaults), standardUserDefaults];
        let value: i64 = msg_send![defaults, integerForKey: ns_string(PROJECT_SORT_KEY)];
        if value == SORT_ALPHABETICAL {
            SORT_ALPHABETICAL
        } else {
            SORT_LAST_DEPLOYMENT
        }
    }
}

pub(crate) fn auto_refresh_minutes() -> u64 {
    unsafe {
        let defaults: ObjcId = msg_send![class!(NSUserDefaults), standardUserDefaults];
        let value: i64 = msg_send![defaults, integerForKey: ns_string(AUTO_REFRESH_KEY)];
        let value = value as u64;
        if AUTO_REFRESH_OPTIONS.contains(&value) {
            value
        } else {
            5
        }
    }
}

fn set_auto_refresh_minutes(minutes: u64) {
    unsafe {
        let defaults: ObjcId = msg_send![class!(NSUserDefaults), standardUserDefaults];
        send_void![
            defaults,
            setInteger: minutes as i64,
            forKey: ns_string(AUTO_REFRESH_KEY),
        ];
        let _: Bool = msg_send![defaults, synchronize];
    }
}

fn set_project_sort_order(order: i64) {
    unsafe {
        let defaults: ObjcId = msg_send![class!(NSUserDefaults), standardUserDefaults];
        send_void![
            defaults,
            setInteger: order,
            forKey: ns_string(PROJECT_SORT_KEY),
        ];
        let _: Bool = msg_send![defaults, synchronize];
    }
}

#[cfg(test)]
mod tests {
    use super::team_checkbox_state;

    #[test]
    fn team_checkbox_reflects_project_selection() {
        assert_eq!(team_checkbox_state(0, 3), 0);
        assert_eq!(team_checkbox_state(3, 3), 1);
        assert_eq!(team_checkbox_state(1, 3), -1);
    }
}
