//! The shared sound in Preferences (0.52.0): whether this computer listens,
//! whether it sends its own sound, and the volume of each computer heard.

use std::{
    cell::{Cell, RefCell},
    collections::HashMap,
    rc::Rc,
    time::Duration,
};

use adw::prelude::*;
use gtk::{glib, glib::clone, subclass::prelude::ObjectSubclassIsExt};
use rustinput_i18n::{tr, trf};
use rustinput_ipc::{ClientHandle, FrontendRequest, SoundSource, SoundState};

use super::Window;

/// the row of one computer heard
struct SourceRow {
    row: adw::ActionRow,
    scale: gtk::Scale,
    mute: gtk::ToggleButton,
    /// the volume waiting to be sent while the scale moves
    pending: Rc<RefCell<Option<glib::SourceId>>>,
}

/// what the window keeps of the group
pub(crate) struct Sound {
    group: adw::PreferencesGroup,
    listen: gtk::Switch,
    send: gtk::Switch,
    keep: gtk::Switch,
    status: adw::ActionRow,
    sources: RefCell<HashMap<ClientHandle, SourceRow>>,
    /// the widgets are being set to what the service has
    updating: Rc<Cell<bool>>,
}

fn switch_row(title: &str, subtitle: &str) -> (adw::ActionRow, gtk::Switch) {
    let row = adw::ActionRow::builder()
        .title(title)
        .subtitle(subtitle)
        .build();
    let switch = gtk::Switch::builder().valign(gtk::Align::Center).build();
    row.add_suffix(&switch);
    row.set_activatable_widget(Some(&switch));
    (row, switch)
}

impl Window {
    /// the group goes before the one of the file transfers
    pub(super) fn setup_sound(&self) {
        let group = adw::PreferencesGroup::builder()
            .title(tr("Sound"))
            .description(tr(
                "Every computer in the headphones of one, each at its own volume",
            ))
            .visible(false)
            .build();
        let (listen_row, listen) = switch_row(
            tr("Hear the other computers here"),
            tr("Their sound plays on this computer; the volume of each is below"),
        );
        let (send_row, send) = switch_row(
            tr("Send the sound of this computer to the one that listens"),
            tr("Only to a computer authorized here, encrypted"),
        );
        let (keep_row, keep) = switch_row(
            tr("Keep the speakers of this computer playing"),
            tr("Off: they are silent while its sound plays on the other computer"),
        );
        let status = adw::ActionRow::builder().visible(false).build();
        status.add_css_class("property");
        for row in [&listen_row, &send_row, &keep_row] {
            group.add(row);
        }
        group.add(&status);
        let files = self.imp().files_group.get();
        if let Some(parent) = files.parent().and_then(|p| p.downcast::<gtk::Box>().ok()) {
            parent.insert_child_after(&group, files.prev_sibling().as_ref());
        }
        let updating = Rc::new(Cell::new(false));
        for (switch, request) in [
            (
                &listen,
                FrontendRequest::SetSoundListen as fn(bool) -> FrontendRequest,
            ),
            (&send, FrontendRequest::SetSoundSend),
            (&keep, FrontendRequest::SetSoundKeepSpeakers),
        ] {
            switch.connect_active_notify(clone!(
                #[weak(rename_to = window)]
                self,
                #[strong]
                updating,
                move |switch| {
                    if !updating.get() {
                        window.request(request(switch.is_active()));
                    }
                }
            ));
        }
        self.imp().sound.replace(Some(Sound {
            group,
            listen,
            send,
            keep,
            status,
            sources: RefCell::new(HashMap::new()),
            updating,
        }));
    }

    /// what the service says about the sound
    pub(crate) fn set_sound(&self, state: SoundState) {
        let imp = self.imp();
        let sound = imp.sound.borrow();
        let Some(sound) = sound.as_ref() else {
            return;
        };
        sound.group.set_visible(state.supported);
        sound.updating.set(true);
        sound.listen.set_active(state.listen);
        sound.send.set_active(state.send);
        sound.keep.set_active(state.keep_speakers);
        // a computer that listens does not send its own sound
        sound.send.set_sensitive(!state.listen);
        sound.keep.set_sensitive(state.send && !state.listen);
        sound.updating.set(false);
        let status = match (&state.error, &state.sending_to) {
            (Some(error), _) => Some(trf("The sound does not work: {error}", &[("error", error)])),
            (None, Some(to)) => Some(trf(
                "The sound of this computer plays on {name}",
                &[("name", to)],
            )),
            (None, None) if state.listen && state.sources.is_empty() => {
                Some(tr("No computer of the list is switched on to send its sound").to_owned())
            }
            _ => None,
        };
        sound.status.set_visible(status.is_some());
        sound
            .status
            .set_title(status.as_deref().unwrap_or_default());

        let mut rows = sound.sources.borrow_mut();
        rows.retain(|handle, row| {
            let keep = state.sources.iter().any(|s| s.handle == *handle);
            if !keep {
                sound.group.remove(&row.row);
            }
            keep
        });
        for source in &state.sources {
            let row = rows
                .entry(source.handle)
                .or_insert_with(|| self.source_row(&sound.group, source, sound.updating.clone()));
            row.row.set_title(&source.name);
            row.row.set_subtitle(&answer_text(source));
            let known = matches!(source.answer, Some(Ok(())));
            row.scale.set_sensitive(known);
            row.mute.set_sensitive(known);
            // not while the volume moves under the pointer
            if row.pending.borrow().is_none() {
                sound.updating.set(true);
                row.scale.set_value(f64::from(source.volume));
                row.mute.set_active(source.muted);
                row.mute.set_icon_name(if source.muted {
                    "audio-volume-muted-symbolic"
                } else {
                    "audio-volume-high-symbolic"
                });
                sound.updating.set(false);
            }
        }
    }

    /// the row of a computer heard, with its volume and its mute
    fn source_row(
        &self,
        group: &adw::PreferencesGroup,
        source: &SoundSource,
        updating: Rc<Cell<bool>>,
    ) -> SourceRow {
        let row = adw::ActionRow::builder().title(&source.name).build();
        let scale = gtk::Scale::with_range(gtk::Orientation::Horizontal, 0.0, 100.0, 1.0);
        scale.set_width_request(180);
        scale.set_draw_value(false);
        scale.set_valign(gtk::Align::Center);
        let mute = gtk::ToggleButton::builder()
            .icon_name("audio-volume-high-symbolic")
            .valign(gtk::Align::Center)
            .tooltip_text(tr("Mute"))
            .build();
        mute.add_css_class("flat");
        row.add_suffix(&scale);
        row.add_suffix(&mute);
        group.add(&row);
        let pending: Rc<RefCell<Option<glib::SourceId>>> = Rc::default();
        let handle = source.handle;
        let send = clone!(
            #[weak(rename_to = window)]
            self,
            #[weak]
            scale,
            #[weak]
            mute,
            move || {
                window.request(FrontendRequest::SetSoundVolume {
                    handle,
                    volume: scale.value().round() as u8,
                    muted: mute.is_active(),
                });
            }
        );
        let send = Rc::new(send);
        scale.connect_value_changed(clone!(
            #[strong]
            pending,
            #[strong]
            send,
            #[strong]
            updating,
            move |_| {
                if updating.get() {
                    return;
                }
                // the volume goes once the scale rests
                if let Some(id) = pending.take() {
                    id.remove();
                }
                let (pending_inner, send) = (pending.clone(), send.clone());
                pending.replace(Some(glib::timeout_add_local_once(
                    Duration::from_millis(300),
                    move || {
                        pending_inner.take();
                        send();
                    },
                )));
            }
        ));
        mute.connect_toggled(clone!(
            #[strong]
            send,
            #[strong]
            updating,
            move |mute| {
                mute.set_icon_name(if mute.is_active() {
                    "audio-volume-muted-symbolic"
                } else {
                    "audio-volume-high-symbolic"
                });
                if !updating.get() {
                    send();
                }
            }
        ));
        SourceRow {
            row,
            scale,
            mute,
            pending,
        }
    }
}

/// what a computer answered to the stream handed out
fn answer_text(source: &SoundSource) -> String {
    match (&source.answer, source.heard) {
        (None, _) => tr("Asking for its sound…").to_owned(),
        (Some(Ok(())), true) => tr("Its sound plays here").to_owned(),
        (Some(Ok(())), false) => tr("It sends its sound; nothing plays there now").to_owned(),
        (Some(Err(why)), _) => trf("It does not send its sound: {why}", &[("why", why)]),
    }
}
