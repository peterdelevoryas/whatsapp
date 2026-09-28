//! The number's contacts: the only people it accepts messages from and the only
//! people it sends to. Configured as `name=number` pairs, comma-separated, e.g.
//! `WHATSAPP_CONTACTS=alice=15551234567,bob=447700900123`.

use anyhow::{Result, bail};

pub struct Contact {
    pub name: String,
    /// Digits with country code, no +, as WhatsApp reports senders.
    pub number: String,
}

pub struct Contacts(Vec<Contact>);

impl Contacts {
    pub fn parse(spec: &str) -> Result<Self> {
        let mut contacts: Vec<Contact> = Vec::new();
        for entry in spec.split(',') {
            let entry = entry.trim();
            if entry.is_empty() {
                continue;
            }
            let Some((name, number)) = entry.split_once('=') else {
                bail!("contact {entry:?} should be name=number");
            };
            let (name, number) = (name.trim(), number.trim());
            if name.is_empty() {
                bail!("contact {entry:?} has no name");
            }
            if number.is_empty() || !number.chars().all(|c| c.is_ascii_digit()) {
                bail!("contact {name}: number must be digits with country code, no +");
            }
            for existing in &contacts {
                if existing.name == name || existing.number == number {
                    bail!("contact {name} is listed twice");
                }
            }
            contacts.push(Contact {
                name: name.to_string(),
                number: number.to_string(),
            });
        }
        if contacts.is_empty() {
            bail!("no contacts configured");
        }
        Ok(Self(contacts))
    }

    pub fn by_name(&self, name: &str) -> Option<&Contact> {
        self.0.iter().find(|c| c.name == name)
    }

    pub fn by_number(&self, number: &str) -> Option<&Contact> {
        self.0.iter().find(|c| c.number == number)
    }

    pub fn names(&self) -> Vec<&str> {
        let mut names = Vec::new();
        for contact in &self.0 {
            names.push(contact.name.as_str());
        }
        names
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_and_looks_up() {
        let contacts = Contacts::parse("alice=15551234567, bob=447700900123").unwrap();
        assert_eq!(contacts.by_name("bob").unwrap().number, "447700900123");
        assert_eq!(contacts.by_number("15551234567").unwrap().name, "alice");
        assert!(contacts.by_name("carol").is_none());
        assert_eq!(contacts.names(), ["alice", "bob"]);
    }

    #[test]
    fn rejects_bad_specs() {
        assert!(Contacts::parse("").is_err());
        assert!(Contacts::parse("15551234567").is_err());
        assert!(Contacts::parse("alice=+15551234567").is_err());
        assert!(Contacts::parse("alice=1,alice=2").is_err());
        assert!(Contacts::parse("alice=1,bob=1").is_err());
    }
}
