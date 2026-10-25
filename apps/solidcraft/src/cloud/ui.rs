//! SolidCraft_Cloud, desktop app: the windows. The Open window lists the designs on the server (or
//! asks for the address and login first); Save As asks for a name on the server. Both are
//! `ui_kit::Window`s, like every window of the app.

use solidcraft_ui_egui::theme::Tokens;
use solidcraft_ui_egui::{SolveApp, documents, ui_kit};

use super::http::Conn;
use super::names::{ago_text, clean_url, lock_text, taken, valid_name, with_extension};
use super::{COPY_HINT, Cloud, FORCE_HINT, Opened, connect, disconnect, force_unlock, now_ms, refresh, size_label, start_open};

/// What a click in the Open window asked for.
enum Act {
    Refresh,
    Computer,
    SignOut,
    Open(String),
    AskForce(String),
    Keep,
    Force(String),
    Connect,
}

impl Cloud {
    /// The windows, once a frame.
    pub fn ui(&self, app: &mut SolveApp, ctx: &egui::Context) {
        self.open_window(app, ctx);
        self.save_window(ctx);
    }

    fn close_open_window(&self) {
        let mut s = self.shared.lock();
        s.panel = false;
        if let Some(a) = s.picking.take() {
            a.resolve(None);
        }
    }

    fn open_window(&self, app: &mut SolveApp, ctx: &egui::Context) {
        if !self.shared.lock().panel {
            return;
        }
        let error = Tokens::get().error;
        let mut act: Option<Act> = None;
        let closed = {
            // The frame draws from a copy, so no lock is held while widgets run.
            let (active, entries, list_error, listing, loading, asked, held, mut form_edit) = {
                let s = self.shared.lock();
                let held: Vec<String> = s.tracked_names_with_lock();
                (
                    s.active,
                    s.entries.clone(),
                    s.list_error.clone(),
                    s.listing,
                    s.loading.clone(),
                    s.unlocking.clone(),
                    held,
                    (s.form.url.clone(), s.form.user.clone(), s.form.password.clone(), s.form.checking, s.form.error.clone()),
                )
            };
            let title = if active { "Design öffnen (Server)" } else { "Mit dem Server verbinden" };
            let closed = ui_kit::Window::new(egui::Id::new("solidcraft_cloud_open"), "open", title).cards(3.5).show_body(ctx, |ui| {
                if !active {
                    ui.label("Adresse des Servers, zum Beispiel cad.deine-domain.de:");
                    ui_kit::cell_editor(ui, egui::Id::new("solidcraft_cloud_url"), &mut form_edit.0, "https://cad.deine-domain.de", false);
                    ui.label("Benutzer:");
                    ui_kit::cell_editor(ui, egui::Id::new("solidcraft_cloud_user"), &mut form_edit.1, "", false);
                    ui.label("Passwort:");
                    let pw = ui.add(egui::TextEdit::singleline(&mut form_edit.2).password(true).desired_width(f32::INFINITY));
                    ui.weak("Das Passwort wird im Schlüsselbund dieses Computers gespeichert.");
                    if let Some(e) = &form_edit.4 {
                        ui.colored_label(error, e);
                    }
                    let enter = pw.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                    ui.horizontal(|ui| {
                        if ui_kit::button(ui, "Verbinden", ui_kit::Button::Enabled(!form_edit.3)).clicked() || (enter && !form_edit.3) {
                            act = Some(Act::Connect);
                        }
                        if form_edit.3 {
                            ui.spinner();
                        }
                        if ui_kit::button(ui, "Vom Computer öffnen…", ui_kit::Button::Normal).clicked() {
                            act = Some(Act::Computer);
                        }
                    });
                    return;
                }
                ui.horizontal(|ui| {
                    if ui_kit::button(ui, "Aktualisieren", ui_kit::Button::Normal).clicked() {
                        act = Some(Act::Refresh);
                    }
                    if ui_kit::button(ui, "Vom Computer öffnen…", ui_kit::Button::Normal)
                        .on_hover_text("Eine Datei von diesem Computer öffnen. Mit „Speichern unter“ kommt sie auf den Server.")
                        .clicked()
                    {
                        act = Some(Act::Computer);
                    }
                    if ui_kit::button(ui, "Abmelden", ui_kit::Button::Normal)
                        .on_hover_text("Den Server vergessen. Die Anmeldung wird gelöscht.")
                        .clicked()
                    {
                        act = Some(Act::SignOut);
                    }
                });
                ui.separator();
                if let Some(e) = &list_error {
                    ui.colored_label(error, e);
                }
                if let Some(name) = &loading {
                    ui.horizontal(|ui| {
                        ui.spinner();
                        ui.label(format!("Lade „{name}“ …"));
                    });
                }
                if entries.is_empty() {
                    if listing {
                        ui.horizontal(|ui| {
                            ui.spinner();
                            ui.label("Lese die Liste vom Server …");
                        });
                    } else if list_error.is_none() {
                        ui.label("Noch keine Designs auf dem Server. Mit „Speichern unter“ legst du eines an.");
                    }
                }
                let ready = loading.is_none();
                let now = now_ms();
                // The window's text is cut to fit; the list's names, dates and buttons are shown whole.
                ui.style_mut().wrap_mode = Some(egui::TextWrapMode::Extend);
                egui::Grid::new("solidcraft_cloud_designs").num_columns(5).striped(true).spacing([12.0, 6.0]).show(ui, |ui| {
                    for e in &entries {
                        let here = held.contains(&e.name);
                        let open_elsewhere = e.lock_owner.is_some() && !here;
                        let open = ui_kit::button(ui, "Öffnen", ui_kit::Button::Enabled(ready));
                        if open_elsewhere {
                            open.clone().on_hover_text(COPY_HINT);
                        }
                        if open.clicked() {
                            act = Some(Act::Open(e.name.clone()));
                        }
                        ui.label(&e.name);
                        ui.label(size_label(e.size));
                        ui.label(ago_text(now, e.modified_ms));
                        match &e.lock_owner {
                            Some(owner) => {
                                ui.horizontal(|ui| {
                                    ui.weak(lock_text(owner, e.mine, here));
                                    if !open_elsewhere {
                                        return;
                                    }
                                    if asked.as_deref() == Some(e.name.as_str()) {
                                        if ui_kit::button(ui, "Wirklich aufheben", ui_kit::Button::Normal).clicked() {
                                            act = Some(Act::Force(e.name.clone()));
                                        }
                                        if ui_kit::button(ui, "Abbrechen", ui_kit::Button::Normal).clicked() {
                                            act = Some(Act::Keep);
                                        }
                                    } else if ui_kit::button(ui, "Sperre aufheben", ui_kit::Button::Normal).on_hover_text(FORCE_HINT).clicked() {
                                        act = Some(Act::AskForce(e.name.clone()));
                                    }
                                });
                            }
                            None => {
                                ui.label("");
                            }
                        }
                        ui.end_row();
                    }
                });
            });
            // What was typed stays.
            if !active {
                let mut s = self.shared.lock();
                s.form.url = form_edit.0;
                s.form.user = form_edit.1;
                s.form.password = form_edit.2;
            }
            closed
        };
        if closed {
            self.close_open_window();
            return;
        }
        let Some(act) = act else { return };
        match act {
            Act::Refresh => refresh(&self.shared),
            Act::Computer => self.open_from_computer(),
            Act::SignOut => disconnect(&self.shared),
            Act::AskForce(name) => self.shared.lock().unlocking = Some(name),
            Act::Keep => self.shared.lock().unlocking = None,
            Act::Force(name) => {
                self.shared.lock().unlocking = None;
                force_unlock(&self.shared, name);
            }
            Act::Connect => {
                let (url, user, password) = {
                    let s = self.shared.lock();
                    (s.form.url.clone(), s.form.user.trim().to_string(), s.form.password.clone())
                };
                match clean_url(&url) {
                    None => self.shared.lock().form.error = Some("Das ist keine Adresse. Beispiel: https://cad.deine-domain.de".into()),
                    Some(_) if user.is_empty() => self.shared.lock().form.error = Some("Bitte einen Benutzer eingeben.".into()),
                    Some(base) => connect(&self.shared, Conn { base, user, password }, true),
                }
            }
            Act::Open(name) => {
                let path = self.shared.cache_path("designs", &name).map(|p| p.to_string_lossy().into_owned());
                // Open already: its tab, not a second copy that would save over it.
                if let Some(p) = path.filter(|p| documents::find_path(app, p).is_some()) {
                    let mut s = self.shared.lock();
                    s.panel = false;
                    if let Some(a) = s.picking.take() {
                        a.resolve(None);
                    }
                    s.opened.push(Opened { path: p, copy: None });
                    return;
                }
                let answer = self.shared.lock().picking.clone();
                start_open(&self.shared, name, answer);
            }
        }
    }

    fn save_window(&self, ctx: &egui::Context) {
        let Some(mut ask) = self.shared.lock().save_ask.take() else { return };
        let error = Tokens::get().error;
        let mut answer: Option<Option<String>> = None;
        let mut on_computer = false;
        let closed = ui_kit::Window::new(egui::Id::new("solidcraft_cloud_save"), "save", "Auf dem Server speichern").show_body(ctx, |ui| {
            ui.label("Name des Designs:");
            let r = ui_kit::cell_editor(ui, egui::Id::new("solidcraft_cloud_save_name"), &mut ask.name, "", ask.error.is_some());
            if std::mem::take(&mut ask.focus) {
                r.request_focus();
            }
            ui.weak("Ohne Endung wird .solidcraft ergänzt.");
            ui.weak("Alle im Team sehen das Design.");
            if let Some(e) = &ask.error {
                ui.colored_label(error, e);
            }
            let enter = r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
            ui.horizontal(|ui| {
                if ui_kit::button(ui, "Speichern", ui_kit::Button::Normal).clicked() || enter {
                    let name = with_extension(&ask.name);
                    let s = self.shared.lock();
                    match valid_name(&name) {
                        Err(e) => ask.error = Some(e),
                        Ok(()) if taken(s.entries.iter().map(|e| e.name.as_str()), &name) => {
                            ask.error = Some(format!("„{name}“ gibt es auf dem Server schon. Bitte einen anderen Namen wählen."));
                        }
                        Ok(()) => answer = Some(Some(name)),
                    }
                }
                if ui_kit::button(ui, "Auf dem Computer speichern…", ui_kit::Button::Normal).clicked() {
                    on_computer = true;
                }
                if ui_kit::button(ui, "Abbrechen", ui_kit::Button::Normal).clicked() || ui.input(|i| i.key_pressed(egui::Key::Escape)) {
                    answer = Some(None);
                }
            });
        });
        if on_computer {
            let name = with_extension(&ask.name);
            self.shared.lock().save_ask = Some(ask);
            self.save_on_computer(&name);
            return;
        }
        match (closed, answer) {
            (true, _) | (_, Some(None)) => ask.answer.resolve(None),
            (_, Some(Some(name))) => match self.expect_new(&name) {
                Some(path) => ask.answer.resolve(Some(path)),
                None => ask.answer.resolve(None),
            },
            (false, None) => self.shared.lock().save_ask = Some(ask),
        }
    }
}
