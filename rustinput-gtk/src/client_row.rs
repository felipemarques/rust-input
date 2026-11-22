mod imp;

use adw::prelude::*;
use adw::subclass::prelude::*;
use gtk::glib::{self, Object};

use rustinput_ipc::{DEFAULT_PORT, LinkStats, Position};

use crate::{
    i18n::{tr, trf},
    link_stats::format_rtt,
};

use super::ClientObject;

glib::wrapper! {
    pub struct ClientRow(ObjectSubclass<imp::ClientRow>)
    @extends gtk::ListBoxRow, gtk::Widget, adw::PreferencesRow, adw::ExpanderRow,
    @implements gtk::Accessible, gtk::Actionable, gtk::Buildable, gtk::ConstraintTarget;
}

impl ClientRow {
    pub fn new(client_object: &ClientObject) -> Self {
        let client_row: Self = Object::builder().build();
        client_row
            .imp()
            .client_object
            .borrow_mut()
            .replace(client_object.clone());
        client_row
    }

    pub fn bind(&self, client_object: &ClientObject) {
        let mut bindings = self.imp().bindings.borrow_mut();

        // bind client active to switch state
        let active_binding = client_object
            .bind_property("active", &self.imp().enable_switch.get(), "state")
            .sync_create()
            .build();

        // bind client active to switch position
        let switch_position_binding = client_object
            .bind_property("active", &self.imp().enable_switch.get(), "active")
            .sync_create()
            .build();

        // bind hostname to hostname edit field
        let hostname_binding = client_object
            .bind_property("hostname", &self.imp().hostname.get(), "text")
            .transform_to(|_, v: Option<String>| {
                if let Some(hostname) = v {
                    Some(hostname)
                } else {
                    Some("".to_string())
                }
            })
            .sync_create()
            .build();

        // bind hostname to title
        let title_binding = client_object
            .bind_property("hostname", self, "title")
            .transform_to(|_, v: Option<String>| {
                v.or_else(|| {
                    Some(format!(
                        "<span font_style=\"italic\" font_weight=\"light\" foreground=\"darkgrey\">{}</span>",
                        glib::markup_escape_text(tr("no name or address"))
                    ))
                })
            })
            .sync_create()
            .build();

        // bind port to port edit field
        let port_binding = client_object
            .bind_property("port", &self.imp().port.get(), "text")
            .transform_to(|_, v: u32| {
                if v == DEFAULT_PORT as u32 {
                    Some("".to_string())
                } else {
                    Some(v.to_string())
                }
            })
            .sync_create()
            .build();

        // bind port to subtitle
        let subtitle_binding = client_object
            .bind_property("port", self, "subtitle")
            .sync_create()
            .build();

        // bind position to selected position
        let position_binding = client_object
            .bind_property("position", &self.imp().position.get(), "selected")
            .transform_to(|_, v: String| match v.as_str() {
                "right" => Some(1u32),
                "top" => Some(2u32),
                "bottom" => Some(3u32),
                _ => Some(0u32),
            })
            .sync_create()
            .build();

        // bind resolving status to spinner visibility
        let resolve_binding = client_object
            .bind_property(
                "resolving",
                &self.imp().dns_loading_indicator.get(),
                "spinning",
            )
            .sync_create()
            .build();

        // bind ips to tooltip-text
        let ip_binding = client_object
            .bind_property("ips", &self.imp().dns_button.get(), "tooltip-text")
            .transform_to(|_, ips: Vec<String>| {
                if ips.is_empty() {
                    Some(tr("No IP addresses for this computer yet").into())
                } else {
                    Some(ips.join("\n"))
                }
            })
            .sync_create()
            .build();

        bindings.push(active_binding);
        bindings.push(switch_position_binding);
        bindings.push(hostname_binding);
        bindings.push(title_binding);
        bindings.push(port_binding);
        bindings.push(subtitle_binding);
        bindings.push(position_binding);
        bindings.push(resolve_binding);
        bindings.push(ip_binding);

        // Render the initial collapsed subtitle from whatever
        // peer_commit the ClientObject was created with. Subsequent
        // changes are pushed by `Window::update_client_state` calling
        // `refresh_version_status` after writing the new property.
        self.refresh_version_status();
    }

    pub fn unbind(&self) {
        for binding in self.imp().bindings.borrow_mut().drain(..) {
            binding.unbind();
        }
    }

    pub fn set_active(&self, active: bool) {
        self.imp().set_active(active);
    }

    pub fn set_hostname(&self, hostname: Option<String>) {
        self.imp().set_hostname(hostname);
    }

    pub fn set_port(&self, port: u16) {
        self.imp().set_port(port);
    }

    pub fn set_position(&self, pos: Position) {
        self.imp().set_pos(pos);
    }

    /// With an arrangement, the arrangement sets the position: the row only
    /// shows it.
    pub fn set_arranged(&self, arranged: bool) {
        let position = &self.imp().position;
        position.set_sensitive(!arranged);
        position.set_subtitle(if arranged {
            tr("Set by the arrangement")
        } else {
            tr("The edge of this screen that leads to it")
        });
    }

    pub fn set_dns_state(&self, resolved: bool) {
        self.imp().set_dns_state(resolved);
    }

    /// The latest second of the input channel with this client, `None` when
    /// it is not connected: the latency next to the title, the charts of the
    /// last minute inside.
    pub fn show_link_stats(&self, stats: Option<&LinkStats>) {
        let imp = self.imp();
        {
            let mut history = imp.history.borrow_mut();
            match stats {
                Some(stats) => history.push(stats),
                None if history.is_empty() => return,
                None => history.clear(),
            }
        }
        let history = imp.history.borrow();
        let connected = !history.is_empty();
        imp.stats_row.set_visible(connected);
        match history.latest_rtt() {
            Some(rtt) => {
                imp.latency_badge.set_label(&format_rtt(rtt));
                imp.latency_badge
                    .set_tooltip_text(Some(tr("Round trip time of the pings to this computer")));
                imp.latency_badge.set_visible(true);
            }
            None => imp.latency_badge.set_visible(false),
        }
        if connected {
            imp.latency_label.set_label(&history.latency_text());
            imp.traffic_label.set_markup(&history.traffic_markup());
            imp.latency_chart.queue_draw();
            imp.traffic_chart.queue_draw();
        }
    }

    /// Recompute the collapsed subtitle (Pango markup) based on the
    /// current `peer-commit` property and the local build's commit.
    /// Soft-warn semantics: a missing or mismatched peer commit
    /// surfaces as orange text but never blocks traffic. Called by
    /// the window after `update_client_state` writes the new
    /// `peer-commit`. The dns-status icon is left to its existing
    /// `set_dns_state` handler so the two indicators don't fight
    /// for the same CSS class.
    pub fn refresh_version_status(&self) {
        let peer: Option<String> = self
            .imp()
            .client_object
            .borrow()
            .as_ref()
            .and_then(|co| co.property::<Option<String>>("peer-commit"));
        let local = crate::local_commit_str();
        self.remove_css_class("peer-paused");
        if self.imp().paused.get() {
            self.remove_css_class("peer-mismatch");
            self.remove_css_class("peer-match");
            self.remove_css_class("peer-unknown");
            self.add_css_class("peer-paused");
            self.set_subtitle(tr(
                "Paused on that computer: it neither controls nor is controlled",
            ));
            return;
        }
        let markup = match peer.as_deref() {
            None => trf(
                "Its version: unknown · this one: {local}",
                &[("local", &local)],
            ),
            Some(p) if p == local.as_str() => {
                trf("Its version: {peer} · the same", &[("peer", &p)])
            }
            Some(p) => trf(
                "Its version: {peer} · this one: {local}",
                &[("peer", &p), ("local", &local)],
            ),
        };
        self.remove_css_class("peer-mismatch");
        self.remove_css_class("peer-match");
        self.remove_css_class("peer-unknown");
        match peer.as_deref() {
            Some(p) if p == local.as_str() => self.add_css_class("peer-match"),
            Some(_) => self.add_css_class("peer-mismatch"),
            None => self.add_css_class("peer-unknown"),
        };
        self.set_subtitle(&markup);
    }
}
