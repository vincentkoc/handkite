//! Bounded XML-only plist framing support. No peer bytes enter diagnostics.

use quick_xml::{Reader, events::Event};
use serde::de::DeserializeOwned;

const DOCTYPE: &[u8] = b"plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\"";

pub(crate) fn encode(value: plist::Dictionary) -> Result<Vec<u8>, ()> {
    let mut body = Vec::new();
    plist::Value::Dictionary(value)
        .to_writer_xml(&mut body)
        .map_err(|_| ())?;
    Ok(body)
}

pub(crate) fn decode<T: DeserializeOwned>(body: &[u8], max: usize) -> Result<T, ()> {
    preflight(body, max)?;
    plist::from_reader_xml(body).map_err(|_| ())
}

pub(crate) fn preflight(body: &[u8], max: usize) -> Result<(), ()> {
    if body.is_empty() || body.len() > max || !body.starts_with(b"<") {
        return Err(());
    }
    // Reject binary length claims and limit XML recursion before deserialization.
    let mut reader = Reader::from_reader(body);
    reader.config_mut().expand_empty_elements = true;
    let mut depth = 0usize;
    let mut elements = 0usize;
    let mut roots = 0usize;
    let mut doctype = false;
    loop {
        match reader.read_event().map_err(|_| ())? {
            Event::Start(_) => {
                if depth == 0 {
                    roots += 1;
                }
                depth += 1;
                elements += 1;
                if depth > 16 || elements > 4096 || roots > 1 {
                    return Err(());
                }
            }
            Event::End(_) => depth = depth.checked_sub(1).ok_or(())?,
            Event::DocType(value) => {
                if doctype || roots != 0 || value.as_ref() != DOCTYPE {
                    return Err(());
                }
                doctype = true;
            }
            Event::Decl(_) if roots != 0 => return Err(()),
            Event::Text(value)
                if depth == 0 && !value.as_ref().iter().all(u8::is_ascii_whitespace) =>
            {
                return Err(());
            }
            // The pinned plist parser can skip these inside strings. Never let
            // deserialization silently change a peer-reported identity.
            Event::CData(_) | Event::GeneralRef(_) => return Err(()),
            Event::PI(_) => return Err(()),
            Event::Eof => break,
            _ => {}
        }
    }
    if depth != 0 || roots != 1 {
        return Err(());
    }
    Ok(())
}
