//! RFC 4791 bounded inventory + multiget. No speculative sync-collection REPORT.
use super::{CaldavClient, CalendarInfo, RawEvent};
use crate::sync_diagnostics::{RequestDiagnostics, SyncFailure};
use anyhow::Result;
use roxmltree::{Document, Node};
use std::{collections::HashMap, time::Duration};

const DAV: &str = "DAV:";
const CAL: &str = "urn:ietf:params:xml:ns:caldav";

fn invalid() -> anyhow::Error {
    SyncFailure::new("invalid_response").into()
}

pub(crate) fn validate_multistatus(text: &str) -> Result<Document<'_>> {
    let doc = Document::parse(text).map_err(|_| invalid())?;
    if !doc.root_element().has_tag_name((DAV, "multistatus")) {
        return Err(invalid());
    }
    if doc
        .root_element()
        .children()
        .filter(|n| n.is_element())
        .any(|n| !n.has_tag_name((DAV, "response")) && !n.has_tag_name((DAV, "sync-token")))
    {
        return Err(invalid());
    }
    for node in doc
        .descendants()
        .filter(|n| n.has_tag_name((DAV, "status")))
    {
        let status = node
            .text()
            .and_then(|s| s.split_whitespace().nth(1))
            .and_then(|s| s.parse::<u16>().ok())
            .ok_or_else(invalid)?;
        // Missing optional properties (e.g. sync-token) are normal on PROPFIND.
        if !(200..300).contains(&status) && status != 404 {
            return Err(SyncFailure::http(status).into());
        }
    }
    Ok(doc)
}

fn property_node<'a>(response: Node<'a, 'a>, ns: &str, name: &str) -> Option<Node<'a, 'a>> {
    response
        .children()
        .filter(|n| n.has_tag_name((DAV, "propstat")))
        .filter(|n| {
            n.children().any(|s| {
                s.has_tag_name((DAV, "status"))
                    && s.text()
                        .is_some_and(|s| s.split_whitespace().nth(1) == Some("200"))
            })
        })
        .flat_map(|n| n.children().filter(|p| p.has_tag_name((DAV, "prop"))))
        .flat_map(|n| n.children())
        .find(|n| n.has_tag_name((ns, name)))
}

fn property<'a>(response: Node<'a, 'a>, ns: &str, name: &str) -> Option<&'a str> {
    property_node(response, ns, name)?.text()
}

pub(crate) fn parse_calendars(text: &str) -> Result<Vec<CalendarInfo>> {
    let doc = validate_multistatus(text)?;
    let mut calendars = Vec::new();
    for response in doc
        .root_element()
        .children()
        .filter(|n| n.has_tag_name((DAV, "response")))
    {
        let resource = property_node(response, DAV, "resourcetype").ok_or_else(invalid)?;
        if !resource
            .children()
            .any(|n| n.has_tag_name((CAL, "calendar")))
        {
            continue;
        }
        let href = href(response)?.to_owned();
        if calendars.iter().any(|c: &CalendarInfo| c.href == href) {
            return Err(invalid());
        }
        calendars.push(CalendarInfo {
            href,
            display_name: property(response, DAV, "displayname").map(str::to_owned),
            color: property(response, "http://apple.com/ns/ical/", "calendar-color")
                .map(str::to_owned),
            ctag: property(response, "http://calendarserver.org/ns/", "getctag").map(str::to_owned),
            sync_token: None,
        });
    }
    Ok(calendars)
}

fn href<'a>(response: Node<'a, 'a>) -> Result<&'a str> {
    response
        .children()
        .find(|n| n.has_tag_name((DAV, "href")))
        .and_then(|n| n.text())
        .filter(|s| !s.is_empty())
        .ok_or_else(invalid)
}

pub(crate) fn parse_inventory(text: &str) -> Result<HashMap<String, String>> {
    let doc = validate_multistatus(text)?;
    let mut result = HashMap::new();
    for response in doc
        .root_element()
        .children()
        .filter(|n| n.has_tag_name((DAV, "response")))
    {
        let tag = property(response, DAV, "getetag")
            .filter(|s| !s.is_empty())
            .ok_or_else(invalid)?;
        if result
            .insert(href(response)?.to_owned(), tag.to_owned())
            .is_some()
        {
            return Err(invalid());
        }
    }
    Ok(result)
}

pub(crate) fn parse_events(text: &str) -> Result<Vec<RawEvent>> {
    let doc = validate_multistatus(text)?;
    let mut result = Vec::new();
    for response in doc
        .root_element()
        .children()
        .filter(|n| n.has_tag_name((DAV, "response")))
    {
        let ical = property(response, CAL, "calendar-data")
            .filter(|s| !s.is_empty())
            .ok_or_else(invalid)?;
        result.push(RawEvent {
            href: href(response)?.to_owned(),
            ical_data: ical.to_owned(),
        });
    }
    Ok(result)
}

fn escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

impl CaldavClient {
    async fn snapshot_report(
        &self,
        href: &str,
        body: String,
        kind: &'static str,
    ) -> Result<String> {
        let request = self.client.request(
            reqwest::Method::from_bytes(b"REPORT")?,
            self.resolve_url(href),
        );
        let response = self
            .apply_auth(request)
            .header("Content-Type", "application/xml; charset=utf-8")
            .header("Depth", "1")
            .timeout(Duration::from_secs(60))
            .body(body)
            .send_observed(kind)
            .await?;
        if response.status().as_u16() != 207 {
            return Err(SyncFailure::http(response.status().as_u16()).into());
        }
        response.text().await
    }

    pub(crate) async fn inventory(
        &self,
        href: &str,
        since: &str,
    ) -> Result<HashMap<String, String>> {
        crate::sync_diagnostics::window_start(since);
        let body = format!(
            r#"<c:calendar-query xmlns:d="DAV:" xmlns:c="urn:ietf:params:xml:ns:caldav">
          <d:prop><d:getetag/></d:prop><c:filter><c:comp-filter name="VCALENDAR">
          <c:comp-filter name="VEVENT"><c:time-range start="{}"/></c:comp-filter>
          </c:comp-filter></c:filter></c:calendar-query>"#,
            escape(since)
        );
        parse_inventory(&self.snapshot_report(href, body, "inventory").await?)
    }

    /// Reject partial multigets and objects modified between inventory and download.
    pub(crate) async fn multiget(
        &self,
        calendar: &str,
        wanted: &[(&String, &String)],
    ) -> Result<Vec<RawEvent>> {
        let hrefs = wanted
            .iter()
            .map(|(href, _)| format!("<d:href>{}</d:href>", escape(href)))
            .collect::<String>();
        let body = format!(
            r#"<c:calendar-multiget xmlns:d="DAV:" xmlns:c="urn:ietf:params:xml:ns:caldav">
          <d:prop><d:getetag/><c:calendar-data/></d:prop>{hrefs}</c:calendar-multiget>"#
        );
        let text = self.snapshot_report(calendar, body, "multiget").await?;
        let tags = parse_inventory(&text)?;
        if tags.len() != wanted.len() || wanted.iter().any(|(h, t)| tags.get(*h) != Some(*t)) {
            return Err(SyncFailure::new("remote_changed").into());
        }
        parse_events(&text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn reject_truncation_html_partial_and_missing_properties() {
        for xml in ["", "<html/>", "<d:multistatus xmlns:d=\"DAV:\">",
            "<d:multistatus xmlns:d=\"DAV:\"><d:response><d:href>/x</d:href><d:status>HTTP/1.1 404 Not Found</d:status></d:response></d:multistatus>"] {
            assert!(parse_inventory(xml).is_err());
            assert!(parse_events(xml).is_err());
        }
        assert!(parse_inventory("<d:multistatus xmlns:d=\"DAV:\"/>")
            .unwrap()
            .is_empty());
    }
}
