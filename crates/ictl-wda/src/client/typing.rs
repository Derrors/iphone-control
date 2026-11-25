//! [`WdaClient`]: The focused field: reading it back, clearing it, typing keys

use super::*;

impl<T: Transport> WdaClient<T> {
    /// The element with keyboard focus (`GET /element/active`); None when no element has it
    pub async fn focused_field(&mut self) -> Result<Option<FocusedField>> {
        let Some(id) = self.active_element().await? else { return Ok(None) };
        let kind = self.attribute(&id, "type").await?;
        let kind = kind.strip_prefix("XCUIElementType").unwrap_or(&kind).to_owned();
        let placeholder = self.attribute(&id, "placeholderValue").await?;
        Ok(Some(FocusedField { id, kind, placeholder }))
    }

    /// The id of the element with keyboard focus (`GET /element/active`), one request; nothing has focus: `None`.
    /// Rereading a field already known ([`Self::focused_field`] takes three) only needs this and its value
    pub async fn active_element(&mut self) -> Result<Option<String>> {
        match self.in_session("GET", "/element/active", None, DEFAULT_TIMEOUT).await {
            Ok(v) => Ok(element_id(&v)),
            Err(WdaError::Remote { kind, .. }) if kind.contains("no such element") => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// An attribute of an element as text; a missing one (`null`) reads as empty
    pub async fn attribute(&mut self, id: &str, name: &str) -> Result<String> {
        let v = self.in_session("GET", &format!("/element/{id}/attribute/{name}"), None, DEFAULT_TIMEOUT).await?;
        Ok(match v {
            Value::String(s) => s,
            Value::Null => String::new(),
            other => other.to_string(),
        })
    }

    /// What the field holds: an empty field reads its placeholder as its value (Spotlight's reads «搜索»), so that
    /// counts as empty, as with native control
    pub async fn field_text(&mut self, f: &FocusedField) -> Result<String> {
        let v = self.attribute(&f.id, "value").await?;
        Ok(if !f.placeholder.is_empty() && v == f.placeholder { String::new() } else { v })
    }

    /// Empties a field (`POST /element/:id/clear`): WDA deletes the text with the keyboard until the field reads
    /// empty, and answers an error when it doesn't (some custom fields)
    pub async fn clear(&mut self, id: &str) -> Result<()> {
        self.in_session("POST", &format!("/element/{id}/clear"), Some(json!({})), Duration::from_secs(60))
            .await
            .map(drop)
    }

    pub async fn keys(&mut self, text: &str) -> Result<()> {
        let chars: Vec<String> = text.chars().map(String::from).collect();
        self.in_session("POST", "/wda/keys", Some(json!({"value": chars})), Duration::from_secs(120)).await.map(drop)
    }
}
