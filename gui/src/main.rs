//! `redmi-buds-gui` — a small libadwaita app for Redmi/Xiaomi buds.

mod worker;

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::mpsc;

use adw::prelude::*;
use gtk::glib;

use redmi_buds::settings::{
    self, CustomEq, EqBand, Gesture, GestureBinding, NoiseMode, EQ_PERSONALIZED, EQ_PRESETS,
    GESTURE_ACTIONS,
};
use worker::{Ev, Req, Snapshot};

const APP_ID: &str = "io.github.creeperkafasi.RedmiBuds";

const CSS: &str = "
.battery-card { padding: 18px 12px; }
.battery-pct { font-size: 28pt; font-weight: 800; font-feature-settings: 'tnum'; }
.battery-card levelbar block.filled { border-radius: 99px; }
.battery-card levelbar trough { min-height: 8px; border-radius: 99px; }
.eq-band { padding: 6px 12px; }
.eq-band label.numeric { font-feature-settings: 'tnum'; }
.eq-actions { padding: 12px; }
";

/// Debounce for slider previews: the RCSP link is slow, so a drag sends one
/// command after the user pauses instead of one per pixel.
const PREVIEW_DELAY: std::time::Duration = std::time::Duration::from_millis(250);

fn main() -> glib::ExitCode {
    let app = adw::Application::builder().application_id(APP_ID).build();
    app.connect_startup(|_| {
        let css = gtk::CssProvider::new();
        css.load_from_string(CSS);
        gtk::style_context_add_provider_for_display(
            &gtk::gdk::Display::default().expect("display"),
            &css,
            gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
        );
    });
    // `--mac` is ours; GTK would reject it as an unknown option.
    let mac = std::env::args().skip_while(|a| a != "--mac").nth(1);
    app.connect_activate(move |app| {
        let mac = mac.clone().or_else(redmi_buds::client::find_paired_buds);
        build(app, mac.unwrap_or_default())
    });
    let argv0: Vec<String> = std::env::args().take(1).collect();
    app.run_with_args(&argv0)
}

/// One battery tile: "Left", big percent, bar, charging line.
struct BatteryCard {
    root: gtk::Box,
    pct: gtk::Label,
    bar: gtk::LevelBar,
    state: gtk::Label,
}

impl BatteryCard {
    fn new(title: &str, icon: &str) -> Self {
        let root = gtk::Box::new(gtk::Orientation::Vertical, 6);
        root.add_css_class("card");
        root.add_css_class("battery-card");
        root.set_hexpand(true);

        let head = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        head.set_halign(gtk::Align::Center);
        head.append(&gtk::Image::from_icon_name(icon));
        let t = gtk::Label::new(Some(title));
        t.add_css_class("heading");
        head.append(&t);

        let pct = gtk::Label::new(Some("–"));
        pct.add_css_class("battery-pct");
        let bar = gtk::LevelBar::for_interval(0.0, 100.0);
        bar.set_margin_start(8);
        bar.set_margin_end(8);
        let state = gtk::Label::new(Some(" "));
        state.add_css_class("dim-label");
        state.add_css_class("caption");

        root.append(&head);
        root.append(&pct);
        root.append(&bar);
        root.append(&state);
        BatteryCard { root, pct, bar, state }
    }

    fn set(&self, level: Option<(i16, bool)>) {
        match level {
            Some((p, charging)) if p >= 0 => {
                self.pct.set_label(&format!("{p}%"));
                self.bar.set_value(p as f64);
                self.state.set_label(if charging { "⚡ Charging" } else { " " });
                self.root.set_opacity(1.0);
            }
            _ => {
                self.pct.set_label("–");
                self.bar.set_value(0.0);
                self.state.set_label("Not available");
                self.root.set_opacity(0.55);
            }
        }
    }
}

/// One expandable gesture row with a left and a right picker.
struct GestureRow {
    gesture: Gesture,
    expander: adw::ExpanderRow,
    left: adw::ComboRow,
    right: adw::ComboRow,
    /// Action id behind each list position (known actions + any unknown current one).
    ids: RefCell<Vec<u8>>,
}

fn action_model(ids: &[u8]) -> gtk::StringList {
    let names: Vec<String> = ids.iter().map(|i| settings::action_name(*i)).collect();
    let refs: Vec<&str> = names.iter().map(String::as_str).collect();
    gtk::StringList::new(&refs)
}

struct Ui {
    tx: mpsc::Sender<Req>,
    toast: adw::ToastOverlay,
    title: adw::WindowTitle,
    stack: gtk::Stack,
    status: adw::StatusPage,
    connect_btn: gtk::Button,
    cards: [BatteryCard; 3],
    eq: adw::ToggleGroup,
    noise_group: adw::PreferencesGroup,
    noise: adw::ToggleGroup,
    /// The preset the toggles currently show, so a slider drag can pick the
    /// personalized one before previewing.
    eq_id: Cell<Option<u8>>,
    custom_group: adw::PreferencesGroup,
    /// One slider per band, built from what the buds report.
    custom_sliders: RefCell<Vec<(u16, gtk::Scale)>>,
    /// The curve as edited in the UI; what Apply stores.
    custom: RefCell<Vec<EqBand>>,
    /// Pending debounced preview.
    preview_timer: RefCell<Option<glib::SourceId>>,
    gestures_group: adw::PreferencesGroup,
    gesture_rows: RefCell<Vec<Rc<GestureRow>>>,
    find_target: adw::ToggleGroup,
    find_btn: gtk::Button,
    ringing: Cell<bool>,
    serial_row: adw::ActionRow,
    mac_row: adw::ActionRow,
    /// Set while the UI is updated from the device, so widgets don't echo writes back.
    syncing: Cell<bool>,
}

impl Ui {
    fn send(&self, r: Req) {
        let _ = self.tx.send(r);
    }

    fn notify(&self, text: &str) {
        self.toast.add_toast(adw::Toast::new(text));
    }

    fn set_battery(&self, b: &[(i16, bool)]) {
        for (i, card) in self.cards.iter().enumerate() {
            card.set(b.get(i).copied());
        }
    }

    fn set_eq(&self, eq: Option<u8>) {
        self.syncing.set(true);
        self.eq_id.set(eq);
        // The toggle list is per model; an unlisted value must not be forced
        // into the group (libadwaita warns and shows nothing selected).
        match eq.filter(|p| EQ_PRESETS.iter().any(|(id, _)| id == p)) {
            Some(p) => self.eq.set_active_name(Some(&p.to_string())),
            None => self.eq.set_active(gtk::INVALID_LIST_POSITION),
        }
        self.syncing.set(false);
    }

    /// Build the band sliders once, then keep them in sync with the device.
    fn set_custom_eq(self: &Rc<Self>, eq: Option<CustomEq>) {
        let Some(eq) = eq.filter(|e| !e.bands.is_empty()) else {
            self.custom_group.set_visible(false);
            return;
        };
        // Building a row calls `set_value`, which emits `value_changed`; guard
        // the whole update so merely opening the window never writes.
        self.syncing.set(true);
        if self.custom_sliders.borrow().is_empty() {
            for band in &eq.bands {
                let row = self.band_row(*band, eq.bound);
                self.custom_group.add(&row);
            }
            let actions = self.custom_actions();
            self.custom_group.add(&actions);
        }
        self.custom_group.set_visible(true);
        self.custom_group.set_description(Some(&format!(
            "{} bands, ±{} dB. Drag a slider to hear it, then Apply to keep it \
             — it plays as the Personalized preset.",
            eq.bands.len(),
            eq.bound
        )));
        for (freq, scale) in self.custom_sliders.borrow().iter() {
            if let Some(band) = eq.bands.iter().find(|b| b.freq == *freq) {
                scale.set_value(band.gain as f64);
            }
        }
        *self.custom.borrow_mut() = eq.bands;
        self.syncing.set(false);
    }

    /// One row: axis label, slider, live dB readout.
    fn band_row(self: &Rc<Self>, band: EqBand, bound: i8) -> gtk::Box {
        let row = gtk::Box::new(gtk::Orientation::Horizontal, 10);
        row.add_css_class("eq-band");

        let name = gtk::Label::new(Some(&band.label()));
        name.set_width_chars(6);
        name.set_xalign(0.0);
        name.add_css_class("dim-label");

        let scale = gtk::Scale::with_range(
            gtk::Orientation::Horizontal,
            -(bound as f64),
            bound as f64,
            1.0,
        );
        scale.set_hexpand(true);
        scale.set_draw_value(false);
        scale.set_round_digits(0);
        scale.add_mark(0.0, gtk::PositionType::Bottom, None);
        scale.set_value(band.gain as f64);

        let value = gtk::Label::new(Some(&format!("{:+}", band.gain)));
        value.set_width_chars(3);
        value.set_xalign(1.0);
        value.add_css_class("numeric");

        let freq = band.freq;
        {
            let ui = Rc::downgrade(self);
            let value = value.clone();
            scale.connect_value_changed(move |s| {
                let gain = s.value().round() as i8;
                value.set_label(&format!("{gain:+}"));
                let Some(ui) = ui.upgrade() else { return };
                if ui.syncing.get() {
                    return;
                }
                if let Some(band) = ui.custom.borrow_mut().iter_mut().find(|b| b.freq == freq) {
                    band.gain = gain;
                }
                ui.schedule_preview();
            });
        }

        row.append(&name);
        row.append(&scale);
        row.append(&value);
        self.custom_sliders.borrow_mut().push((freq, scale));
        row
    }

    /// Reset / Apply buttons under the sliders.
    fn custom_actions(self: &Rc<Self>) -> gtk::Box {
        let box_ = gtk::Box::new(gtk::Orientation::Horizontal, 12);
        box_.add_css_class("eq-actions");
        box_.set_halign(gtk::Align::End);

        let reset = gtk::Button::builder()
            .label("Reset")
            .tooltip_text("Back to a flat curve")
            .css_classes(["pill"])
            .build();
        let apply = gtk::Button::builder()
            .label("Apply")
            .tooltip_text("Store this curve and select Personalized")
            .css_classes(["pill", "suggested-action"])
            .build();

        {
            let ui = Rc::downgrade(self);
            reset.connect_clicked(move |_| {
                let Some(ui) = ui.upgrade() else { return };
                ui.cancel_preview();
                ui.send(Req::ResetCustomEq);
            });
        }
        {
            let ui = Rc::downgrade(self);
            apply.connect_clicked(move |_| {
                let Some(ui) = ui.upgrade() else { return };
                ui.cancel_preview();
                let bands = ui.custom.borrow().clone();
                if !bands.is_empty() {
                    ui.send(Req::SetCustomEq(bands));
                }
            });
        }

        box_.append(&reset);
        box_.append(&apply);
        box_
    }

    /// Queue a preview once the sliders settle. Preview alone does not change
    /// the sound unless the personalized preset is selected, so pick it first
    /// (the preview that follows overwrites the stored curve it reloads).
    fn schedule_preview(self: &Rc<Self>) {
        if self.eq_id.get() != Some(EQ_PERSONALIZED) {
            self.eq_id.set(Some(EQ_PERSONALIZED));
            self.send(Req::SetEq(EQ_PERSONALIZED));
        }
        self.cancel_preview();
        let ui = self.clone();
        let id = glib::timeout_add_local_once(PREVIEW_DELAY, move || {
            ui.preview_timer.borrow_mut().take();
            let bands = ui.custom.borrow().clone();
            if !bands.is_empty() {
                ui.send(Req::PreviewCustomEq(bands));
            }
        });
        *self.preview_timer.borrow_mut() = Some(id);
    }

    fn cancel_preview(&self) {
        if let Some(id) = self.preview_timer.borrow_mut().take() {
            id.remove();
        }
    }

    /// Noise-control mode comes back as `(mode, level)`; only the mode has a
    /// toggle. Unknown values simply leave the row empty.
    fn set_noise(&self, anc: Option<(u8, u8)>) {
        self.syncing.set(true);
        match anc.and_then(|(mode, _)| NoiseMode::from_id(mode)) {
            Some(m) => self.noise.set_active_name(Some(&(m as u8).to_string())),
            None => self.noise.set_active(gtk::INVALID_LIST_POSITION),
        }
        self.syncing.set(false);
    }

    fn set_gestures(self: &Rc<Self>, bindings: &[GestureBinding]) {        self.syncing.set(true);
        // Build rows once; afterwards only refresh selections.
        if self.gesture_rows.borrow().is_empty() {
            for b in bindings {
                let row = self.gesture_row(b.gesture);
                self.gestures_group.add(&row.expander);
                self.gesture_rows.borrow_mut().push(row);
            }
        }
        for row in self.gesture_rows.borrow().iter() {
            if let Some(b) = bindings.iter().find(|b| b.gesture == row.gesture) {
                let mut ids: Vec<u8> = GESTURE_ACTIONS.iter().map(|(i, _)| *i).collect();
                for v in [b.left, b.right] {
                    if !ids.contains(&v) {
                        ids.push(v);
                    }
                }
                if *row.ids.borrow() != ids {
                    let model = action_model(&ids);
                    row.left.set_model(Some(&model));
                    row.right.set_model(Some(&model));
                    *row.ids.borrow_mut() = ids.clone();
                }
                let pos = |v: u8| ids.iter().position(|i| *i == v).unwrap_or(0) as u32;
                row.left.set_selected(pos(b.left));
                row.right.set_selected(pos(b.right));
                row.expander.set_subtitle(&summary(b.left, b.right));
            }
        }
        self.syncing.set(false);
    }

    fn gesture_row(self: &Rc<Self>, gesture: Gesture) -> Rc<GestureRow> {
        let expander = adw::ExpanderRow::builder().title(gesture.name()).build();
        expander.add_prefix(&gtk::Image::from_icon_name(match gesture {
            Gesture::LongPress => "touchpad-disabled-symbolic",
            _ => "input-touchpad-symbolic",
        }));
        let left = adw::ComboRow::builder().title("Left bud").build();
        let right = adw::ComboRow::builder().title("Right bud").build();
        expander.add_row(&left);
        expander.add_row(&right);
        let row = Rc::new(GestureRow {
            gesture,
            expander,
            left,
            right,
            ids: RefCell::new(Vec::new()),
        });
        for is_left in [true, false] {
            let combo = if is_left { &row.left } else { &row.right };
            let ui = Rc::downgrade(self);
            let r = Rc::downgrade(&row);
            combo.connect_selected_notify(move |c| {
                let (Some(ui), Some(r)) = (ui.upgrade(), r.upgrade()) else { return };
                if ui.syncing.get() {
                    return;
                }
                let Some(&id) = r.ids.borrow().get(c.selected() as usize) else { return };
                let (l, rt) = if is_left { (Some(id), None) } else { (None, Some(id)) };
                ui.send(Req::SetGesture(r.gesture, l, rt));
            });
        }
        row
    }

    fn set_ringing(&self, ringing: bool) {
        self.ringing.set(ringing);
        if ringing {
            self.find_btn.set_label("Stop");
            self.find_btn.remove_css_class("suggested-action");
            self.find_btn.add_css_class("destructive-action");
        } else {
            self.find_btn.set_label("Play sound");
            self.find_btn.remove_css_class("destructive-action");
            self.find_btn.add_css_class("suggested-action");
        }
    }

    fn handle(self: &Rc<Self>, ev: Ev) {
        match ev {
            Ev::Connecting => {
                self.title.set_subtitle("Connecting…");
                if self.stack.visible_child_name().as_deref() != Some("main") {
                    self.stack.set_visible_child_name("connecting");
                }
            }
            Ev::Connected(s) => self.show(s),
            Ev::Battery(b) => self.set_battery(&b),
            Ev::Eq(e) => self.set_eq(e),
            Ev::Anc(a) => self.set_noise(a),
            Ev::CustomEq(eq, selected) => {
                self.set_custom_eq(eq);
                if selected {
                    self.set_eq(Some(EQ_PERSONALIZED));
                }
                self.notify(if selected {
                    "Custom equalizer applied"
                } else {
                    "Custom equalizer reset"
                });
            }
            Ev::Gestures(g) => {
                self.set_gestures(&g);
                self.notify("Gesture saved");
            }
            Ev::Failed(msg) => self.notify(&format!("Couldn't apply: {msg}")),
            Ev::Disconnected(msg) => {
                self.set_ringing(false);
                self.title.set_subtitle("Not connected");
                self.status.set_description(Some(&msg));
                self.connect_btn.set_sensitive(true);
                self.stack.set_visible_child_name("offline");
            }
        }
    }

    fn show(self: &Rc<Self>, s: Snapshot) {
        self.title.set_title(&s.name);
        self.title.set_subtitle("Connected");
        self.set_battery(&s.battery);
        self.set_eq(s.eq);
        self.noise_group.set_visible(s.has_anc);
        self.set_noise(s.anc);
        self.set_custom_eq(s.custom_eq);
        self.set_gestures(&s.gestures);
        self.serial_row
            .set_subtitle(s.serial.as_deref().unwrap_or("Unknown"));
        self.mac_row.set_subtitle(&s.mac);
        self.stack.set_visible_child_name("main");
    }
}

fn summary(l: u8, r: u8) -> String {
    if l == r {
        settings::action_name(l)
    } else {
        format!("L: {} · R: {}", settings::action_name(l), settings::action_name(r))
    }
}

/// Toggle labels; the full names don't fit side by side on a narrow window.
fn short_eq_name(id: u8) -> Option<&'static str> {
    Some(match id {
        5 => "Bass",
        6 => "Treble",
        7 => "Loud",
        10 => "Personal",
        _ => return None,
    })
}

fn group(title: &str) -> adw::PreferencesGroup {
    adw::PreferencesGroup::builder().title(title).build()
}

fn build(app: &adw::Application, mac: String) {

    let (tx, rx) = mpsc::channel();
    let (ev_tx, ev_rx) = async_channel::unbounded();
    {
        let mac = mac.clone();
        std::thread::spawn(move || worker::run(mac, rx, ev_tx));
    }

    // Header
    let title = adw::WindowTitle::new("Redmi Buds", "Connecting…");
    let header = adw::HeaderBar::builder().title_widget(&title).build();
    let refresh = gtk::Button::from_icon_name("view-refresh-symbolic");
    refresh.set_tooltip_text(Some("Refresh"));
    header.pack_start(&refresh);

    // Battery
    let cards = [
        BatteryCard::new("Left", "audio-headphones-symbolic"),
        BatteryCard::new("Right", "audio-headphones-symbolic"),
        BatteryCard::new("Case", "battery-symbolic"),
    ];
    let battery_box = gtk::Box::new(gtk::Orientation::Horizontal, 12);
    battery_box.set_homogeneous(true);
    for c in &cards {
        battery_box.append(&c.root);
    }

    // Sound
    let sound = group("Sound");
    sound.set_description(Some("Choose how your buds sound"));
    let eq = adw::ToggleGroup::new();
    eq.add_css_class("round");
    eq.set_hexpand(true);
    for (id, name) in EQ_PRESETS {
        eq.add(
            adw::Toggle::builder()
                .label(short_eq_name(id).unwrap_or(name))
                .tooltip(name)
                .name(id.to_string())
                .build(),
        );
    }
    let eq_scroll = gtk::ScrolledWindow::builder()
        .vscrollbar_policy(gtk::PolicyType::Never)
        .hscrollbar_policy(gtk::PolicyType::Automatic)
        .child(&eq)
        .build();
    eq_scroll.set_propagate_natural_height(true);
    sound.add(&eq_scroll);

    // Custom equalizer: rows are added once the device reports its bands.
    let custom_group = group("Custom equalizer");
    custom_group.set_visible(false);

    // Noise control
    let noise_group = group("Noise control");
    noise_group.set_description(Some("What the buds do with outside sound"));
    noise_group.set_visible(false);
    let noise = adw::ToggleGroup::new();
    noise.add_css_class("round");
    noise.set_hexpand(true);
    for mode in NoiseMode::ALL {
        noise.add(
            adw::Toggle::builder()
                .label(match mode {
                    NoiseMode::Off => "Off",
                    NoiseMode::Cancelling => "Noise cancelling",
                    NoiseMode::Transparency => "Transparency",
                })
                .name((mode as u8).to_string())
                .build(),
        );
    }
    noise_group.add(&noise);

    // Gestures
    let gestures_group = group("Touch controls");
    gestures_group.set_description(Some("What happens when you tap your buds"));

    // Find
    let find = group("Find my buds");
    let find_row = adw::ActionRow::builder()
        .title("Play a sound")
        .subtitle("Make your buds ring loudly. Take them out of your ears first!")
        .build();
    find_row.add_prefix(&gtk::Image::from_icon_name("find-location-symbolic"));
    let find_target = adw::ToggleGroup::new();
    find_target.set_valign(gtk::Align::Center);
    for (name, label) in [("1", "Left"), ("3", "Both"), ("2", "Right")] {
        find_target.add(adw::Toggle::builder().label(label).name(name).build());
    }
    find_target.set_active_name(Some("3"));
    let find_btn = gtk::Button::builder()
        .label("Play sound")
        .valign(gtk::Align::Center)
        .css_classes(["pill", "suggested-action"])
        .build();
    let find_box = gtk::Box::new(gtk::Orientation::Horizontal, 12);
    find_box.set_margin_top(12);
    find_box.set_halign(gtk::Align::Center);
    find_box.append(&find_target);
    find_box.append(&find_btn);
    find.add(&find_row);
    find.add(&find_box);

    // About
    let about = group("Device");
    let serial_row = adw::ActionRow::builder().title("Serial number").subtitle("–").build();
    serial_row.add_css_class("property");
    serial_row.set_subtitle_selectable(true);
    let mac_row = adw::ActionRow::builder().title("Bluetooth address").subtitle(&mac).build();
    mac_row.add_css_class("property");
    mac_row.set_subtitle_selectable(true);
    about.add(&serial_row);
    about.add(&mac_row);

    let content = gtk::Box::new(gtk::Orientation::Vertical, 24);
    content.set_margin_top(18);
    content.set_margin_bottom(24);
    content.set_margin_start(12);
    content.set_margin_end(12);
    content.append(&battery_box);
    for g in [&sound, &custom_group, &noise_group, &gestures_group, &find, &about] {
        content.append(g);
    }
    let clamp = adw::Clamp::builder().maximum_size(640).child(&content).build();
    let main_page = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .child(&clamp)
        .build();

    // Connecting / offline pages
    let spinner = adw::Spinner::new();
    spinner.set_size_request(48, 48);
    let connecting = adw::StatusPage::builder()
        .title("Connecting to your buds")
        .description("Open the case or put the buds in your ears")
        .child(&spinner)
        .build();
    let connect_btn = gtk::Button::builder()
        .label("Try again")
        .halign(gtk::Align::Center)
        .css_classes(["pill", "suggested-action"])
        .build();
    let status = adw::StatusPage::builder()
        .icon_name("audio-headphones-symbolic")
        .title("Buds not connected")
        .child(&connect_btn)
        .build();

    let stack = gtk::Stack::builder()
        .transition_type(gtk::StackTransitionType::Crossfade)
        .build();
    stack.add_named(&connecting, Some("connecting"));
    stack.add_named(&status, Some("offline"));
    stack.add_named(&main_page, Some("main"));
    stack.set_visible_child_name("connecting");

    let toast = adw::ToastOverlay::new();
    toast.set_child(Some(&stack));
    let view = adw::ToolbarView::new();
    view.add_top_bar(&header);
    view.set_content(Some(&toast));

    let window = adw::ApplicationWindow::builder()
        .application(app)
        .title("Redmi Buds")
        .default_width(520)
        .default_height(820)
        .width_request(360)
        .height_request(400)
        .content(&view)
        .build();

    let ui = Rc::new(Ui {
        tx,
        toast,
        title,
        stack,
        status,
        connect_btn,
        cards,
        eq,
        noise_group,
        noise,
        eq_id: Cell::new(None),
        custom_group,
        custom_sliders: RefCell::new(Vec::new()),
        custom: RefCell::new(Vec::new()),
        preview_timer: RefCell::new(None),
        gestures_group,
        gesture_rows: RefCell::new(Vec::new()),
        find_target,
        find_btn,
        ringing: Cell::new(false),
        serial_row,
        mac_row,
        syncing: Cell::new(false),
    });

    // Signals
    {
        let u = Rc::downgrade(&ui);
        refresh.connect_clicked(move |_| {
            if let Some(u) = u.upgrade() {
                u.send(Req::Refresh);
            }
        });
    }
    {
        let u = Rc::downgrade(&ui);
        ui.connect_btn.connect_clicked(move |b| {
            if let Some(u) = u.upgrade() {
                b.set_sensitive(false);
                u.send(Req::Connect);
            }
        });
    }
    {
        let u = Rc::downgrade(&ui);
        ui.eq.connect_active_name_notify(move |g| {
            let Some(u) = u.upgrade() else { return };
            if u.syncing.get() {
                return;
            }
            if let Some(p) = g.active_name().and_then(|n| n.parse().ok()) {
                u.send(Req::SetEq(p));
            }
        });
    }
    {
        let u = Rc::downgrade(&ui);
        ui.noise.connect_active_name_notify(move |g| {
            let Some(u) = u.upgrade() else { return };
            if u.syncing.get() {
                return;
            }
            if let Some(mode) = g.active_name().and_then(|n| n.parse().ok()) {
                u.send(Req::SetAnc(mode));
            }
        });
    }
    {
        let u = Rc::downgrade(&ui);
        ui.find_btn.connect_clicked(move |_| {
            let Some(u) = u.upgrade() else { return };
            let ring = !u.ringing.get();
            let device = u
                .find_target
                .active_name()
                .and_then(|n| n.parse().ok())
                .unwrap_or(3);
            u.send(Req::Find { ring, device });
            u.set_ringing(ring);
        });
    }

    // Device events
    {
        let ui = ui.clone();
        glib::spawn_future_local(async move {
            while let Ok(ev) = ev_rx.recv().await {
                ui.handle(ev);
            }
        });
    }

    // Stop a ringing bud if the window is closed mid-ring.
    {
        let u = Rc::downgrade(&ui);
        window.connect_close_request(move |_| {
            if let Some(u) = u.upgrade() {
                if u.ringing.get() {
                    let device = u.find_target.active_name().and_then(|n| n.parse().ok()).unwrap_or(3);
                    u.send(Req::Find { ring: false, device });
                    std::thread::sleep(std::time::Duration::from_millis(300));
                }
            }
            glib::Propagation::Proceed
        });
    }

    window.present();
}
