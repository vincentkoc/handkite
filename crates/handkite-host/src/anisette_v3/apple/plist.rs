use std::collections::BTreeMap;

use quick_xml::{
    Reader,
    events::{BytesStart, Event},
};
use zeroize::Zeroizing;

const INVALID: &str = "Apple provisioning plist is invalid";
const MAX_DEPTH: usize = 16;
const MAX_ELEMENTS: usize = 4096;
const STANDARD_DOCTYPE: &[u8] = b"plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\"";

pub(super) enum Value {
    Dictionary(BTreeMap<String, Value>),
    String(Zeroizing<String>),
    Integer(i128),
    Other,
}

impl Value {
    pub(super) fn field(&self, key: &str) -> Result<&Self, &'static str> {
        match self {
            Self::Dictionary(values) => values.get(key).ok_or(INVALID),
            _ => Err(INVALID),
        }
    }

    pub(super) fn string(&self) -> Result<&str, &'static str> {
        match self {
            Self::String(value) => Ok(value),
            _ => Err(INVALID),
        }
    }

    pub(super) fn integer(&self) -> Result<i64, &'static str> {
        match self {
            Self::Integer(value) => i64::try_from(*value).map_err(|_| INVALID),
            _ => Err(INVALID),
        }
    }
}

pub(super) fn parse(body: &[u8], max_bytes: usize) -> Result<Value, &'static str> {
    if body.is_empty() || body.len() > max_bytes {
        return Err(INVALID);
    }
    let text = std::str::from_utf8(body).map_err(|_| INVALID)?;
    if !text.chars().all(xml_character) {
        return Err(INVALID);
    }
    let mut reader = Reader::from_reader(body);
    reader.config_mut().expand_empty_elements = true;
    reader.config_mut().check_comments = true;
    let mut parser = Parser {
        reader,
        elements: 0,
    };
    let mut declaration = false;
    let mut doctype = false;
    let mut before_declaration = true;
    loop {
        match parser.reader.read_event().map_err(|_| INVALID)? {
            Event::Decl(value) if before_declaration && !declaration && !doctype => {
                let content = std::str::from_utf8(value.as_ref()).map_err(|_| INVALID)?;
                let attributes = BytesStart::from_content(content, 3);
                let mut version = false;
                for attribute in attributes.attributes() {
                    let attribute = attribute.map_err(|_| INVALID)?;
                    match (attribute.key.as_ref(), attribute.value.as_ref()) {
                        (b"version", b"1.0") if !version => version = true,
                        (b"encoding", b"UTF-8" | b"utf-8") if version => {}
                        (b"standalone", b"yes" | b"no") if version => {}
                        _ => return Err(INVALID),
                    }
                }
                if !version {
                    return Err(INVALID);
                }
                declaration = true;
            }
            Event::DocType(value) if !doctype && value.as_ref() == STANDARD_DOCTYPE => {
                // Recognize this declaration as inert syntax. No DTD is loaded.
                doctype = true;
            }
            Event::Text(value) if value.as_ref().iter().all(u8::is_ascii_whitespace) => {}
            Event::Comment(_) => {}
            Event::Start(value) if value.name().as_ref() == b"plist" => {
                let mut version = false;
                for attribute in value.attributes() {
                    let attribute = attribute.map_err(|_| INVALID)?;
                    if attribute.key.as_ref() != b"version" || attribute.value.as_ref() != b"1.0" {
                        return Err(INVALID);
                    }
                    version = true;
                }
                if !version {
                    return Err(INVALID);
                }
                let first = parser.structure()?;
                let root = parser.value(first, 1)?;
                if !matches!(root, Value::Dictionary(_))
                    || !matches!(parser.structure()?, Event::End(end) if end.name().as_ref() == b"plist")
                    || !matches!(parser.structure()?, Event::Eof)
                {
                    return Err(INVALID);
                }
                return Ok(root);
            }
            _ => return Err(INVALID),
        }
        before_declaration = false;
    }
}

struct Parser<'a> {
    reader: Reader<&'a [u8]>,
    elements: usize,
}

impl<'a> Parser<'a> {
    fn structure(&mut self) -> Result<Event<'a>, &'static str> {
        loop {
            match self.reader.read_event().map_err(|_| INVALID)? {
                Event::Text(value) if value.as_ref().iter().all(u8::is_ascii_whitespace) => {}
                Event::Comment(_) => {}
                event => return Ok(event),
            }
        }
    }

    fn start(&mut self, event: Event<'a>) -> Result<BytesStart<'a>, &'static str> {
        self.elements += 1;
        if self.elements > MAX_ELEMENTS {
            return Err(INVALID);
        }
        match event {
            Event::Start(start) if start.attributes().next().is_none() => Ok(start),
            _ => Err(INVALID),
        }
    }

    fn value(&mut self, event: Event<'a>, depth: usize) -> Result<Value, &'static str> {
        if depth > MAX_DEPTH {
            return Err(INVALID);
        }
        let start = self.start(event)?;
        match start.name().as_ref() {
            b"dict" => {
                let mut values = BTreeMap::new();
                loop {
                    let event = self.structure()?;
                    if matches!(&event, Event::End(end) if end.name().as_ref() == b"dict") {
                        return Ok(Value::Dictionary(values));
                    }
                    let key = self.start(event)?;
                    if key.name().as_ref() != b"key" {
                        return Err(INVALID);
                    }
                    let key = self.text(b"key")?;
                    if values.contains_key(key.as_str()) {
                        return Err(INVALID);
                    }
                    let event = self.structure()?;
                    let value = self.value(event, depth + 1)?;
                    values.insert(key.to_string(), value);
                }
            }
            b"array" => loop {
                let event = self.structure()?;
                if matches!(&event, Event::End(end) if end.name().as_ref() == b"array") {
                    return Ok(Value::Other);
                }
                self.value(event, depth + 1)?;
            },
            b"string" => Ok(Value::String(self.text(b"string")?)),
            b"integer" => {
                let text = self.text(b"integer")?;
                let text = text.trim();
                let value = text
                    .parse::<i64>()
                    .map(i128::from)
                    .or_else(|_| text.parse::<u64>().map(i128::from))
                    .map_err(|_| INVALID)?;
                Ok(Value::Integer(value))
            }
            b"true" | b"false" => {
                if !self.text(start.name().as_ref())?.is_empty() {
                    return Err(INVALID);
                }
                Ok(Value::Other)
            }
            // Unsupported plist types fail closed instead of silently changing their meaning.
            _ => Err(INVALID),
        }
    }

    fn text(&mut self, tag: &[u8]) -> Result<Zeroizing<String>, &'static str> {
        let mut result = Zeroizing::new(String::new());
        loop {
            match self.reader.read_event().map_err(|_| INVALID)? {
                Event::Text(value) => {
                    if value.as_ref().windows(3).any(|part| part == b"]]>") {
                        return Err(INVALID);
                    }
                    result.push_str(&value.xml10_content().map_err(|_| INVALID)?);
                }
                Event::CData(value) => {
                    result.push_str(&value.xml10_content().map_err(|_| INVALID)?)
                }
                Event::GeneralRef(value) => {
                    let character = match value.resolve_char_ref().map_err(|_| INVALID)? {
                        Some(character) if xml_character(character) => character,
                        Some(_) => return Err(INVALID),
                        None => match value.as_ref() {
                            b"amp" => '&',
                            b"lt" => '<',
                            b"gt" => '>',
                            b"apos" => '\'',
                            b"quot" => '"',
                            _ => return Err(INVALID),
                        },
                    };
                    result.push(character);
                }
                Event::Comment(_) => {}
                Event::End(end) if end.name().as_ref() == tag => return Ok(result),
                _ => return Err(INVALID),
            }
        }
    }
}

fn xml_character(character: char) -> bool {
    matches!(character, '\t' | '\n' | '\r' | '\u{20}'..='\u{d7ff}' | '\u{e000}'..='\u{fffd}' | '\u{10000}'..='\u{10ffff}')
}
