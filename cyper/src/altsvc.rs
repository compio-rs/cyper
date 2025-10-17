//! Vendored and modified from `altsvc` crate.

use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::Instant,
};

use thiserror::Error;

#[derive(Error, Debug)]
pub enum ParseError {
    #[error("invalid parameter: {0}")]
    Parameter(String),
    #[error("invalid value of 'ma': {0}")]
    MaValue(String),
    #[error("invalid value of 'persist': {0}")]
    PersistValue(String),
    #[error("invalid value of 'alt-authority': {0}")]
    AltAuthorityValue(String),
    #[error("invalid port number: {0}")]
    PortNumber(String),
}

#[derive(Debug)]
pub enum AltService {
    Clear,
    Services(Vec<Service>),
}

#[derive(Debug, Default)]
pub struct Service {
    id: String,
    authority: AltAuthority,
    max_age: Option<u64>,
    persist: bool,
}

#[derive(Debug, Default)]
pub struct AltAuthority {
    pub host: String,
    pub port: u16,
}

pub fn parse(s: &str) -> Result<AltService, ParseError> {
    fn parse_service(svc_string: &str) -> Result<Service, ParseError> {
        let mut svc = Service::default();

        for kv in svc_string
            .split(';')
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            let [k, v] = kv.split('=').collect::<Vec<_>>()[..] else {
                return Err(ParseError::Parameter(kv.to_string()));
            };

            match k {
                "ma" => {
                    let ma = v.parse().map_err(|_| ParseError::MaValue(v.to_string()))?;
                    svc.max_age = Some(ma);
                }
                "persist"
                    if v.parse::<i32>()
                        .map_err(|_| ParseError::PersistValue(v.to_string()))?
                        == 1 =>
                {
                    svc.persist = true;
                }
                "persist" => (),
                _ => {
                    let value = v.trim_matches('"');
                    let [host, port_str] = value.splitn(2, ':').collect::<Vec<_>>()[..] else {
                        return Err(ParseError::AltAuthorityValue(value.to_string()));
                    };

                    svc.id = k.to_string();
                    svc.authority = AltAuthority {
                        host: host.to_string(),
                        port: port_str
                            .parse()
                            .map_err(|_| ParseError::PortNumber(port_str.to_string()))?,
                    };
                }
            }
        }

        Ok(svc)
    }

    if s == "clear" {
        return Ok(AltService::Clear);
    }

    let services = s
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(parse_service)
        .collect::<Result<Vec<_>, _>>();

    services.map(AltService::Services)
}

#[derive(Debug)]
struct AltHostEntry {
    insert_time: Instant,
    max_age: u64,
}

#[derive(Debug, Clone, Default)]
pub struct KnownHosts {
    map: Arc<Mutex<HashMap<String, AltHostEntry>>>,
}

impl KnownHosts {
    pub fn try_insert(&self, host: String, srv: &crate::altsvc::Service) -> bool {
        if srv.id == "h3"
            && (srv.authority.host.is_empty() || srv.authority.host == host)
            && srv.authority.port == 443
        {
            let mut map = self.map.lock().unwrap();
            map.insert(
                host,
                AltHostEntry {
                    insert_time: Instant::now(),
                    max_age: srv.max_age.unwrap_or(86400), // 24 hours
                },
            );

            return true;
        }

        false
    }

    pub fn find(&self, host: &str) -> bool {
        let mut map = self.map.lock().unwrap();
        if let Some((host, entry)) = map.remove_entry(host)
            && (Instant::now() - entry.insert_time).as_secs() <= entry.max_age
        {
            map.insert(host, entry);
            return true;
        }

        false
    }

    pub fn clear(&self, host: &str) {
        self.map.lock().unwrap().remove(host);
    }
}
