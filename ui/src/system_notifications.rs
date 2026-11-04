//! Native Linux notifications without requiring an installed desktop launcher.
use glib::variant::ToVariant;
use gtk::{gio, glib};
use std::collections::HashMap;

pub async fn show(title: &str, body: &str) -> anyhow::Result<u32> {
    let connection = gio::bus_get_future(gio::BusType::Session).await?;
    let actions: Vec<String> = Vec::new();
    let hints: HashMap<String, glib::Variant> = HashMap::new();
    let body = glib::markup_escape_text(body);
    let parameters = (
        "HyperClix",
        0u32,
        "phone-symbolic",
        title,
        body.as_str(),
        actions,
        hints,
        -1i32,
    )
        .to_variant();
    let reply = connection
        .call_future(
            Some("org.freedesktop.Notifications"),
            "/org/freedesktop/Notifications",
            "org.freedesktop.Notifications",
            "Notify",
            Some(&parameters),
            Some(glib::VariantTy::new("(u)")?),
            gio::DBusCallFlags::NONE,
            3000,
        )
        .await?;
    Ok(reply
        .get::<(u32,)>()
        .ok_or_else(|| anyhow::anyhow!("notification service returned an invalid reply"))?
        .0)
}
