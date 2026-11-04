//! Shared zbus helpers for BlueZ and NetworkManager proxies.
use std::collections::HashMap;
use zbus::{Connection, Proxy, zvariant::OwnedValue};

pub(crate) type Properties = HashMap<String, OwnedValue>;

pub(crate) async fn proxy<'a>(
    bus: &'a Connection,
    service: &'static str,
    path: &'a str,
    interface: &'static str,
) -> zbus::Result<Proxy<'a>> {
    Proxy::new(bus, service, path, interface).await
}

pub(crate) fn string(properties: &Properties, key: &str) -> Option<String> {
    properties
        .get(key)
        .and_then(|value| <&str>::try_from(value).ok())
        .map(str::to_owned)
}
