//! The snapshot index and the lookup (spec §8).

use std::sync::Arc;

use crate::compact::{Name, SortedMap};
use crate::discovery::egress::EgressSet;
use crate::discovery::histogram::Histogram;
use crate::discovery::redact::Redactor;

use super::{Answer, Hit, Miss, Request, Response, Tier};

#[derive(Debug)]
pub struct Index {
    authorities: SortedMap<Name, Vec<Histogram>>,
}

impl Index {
    pub fn build(set: EgressSet) -> Index {
        let _ = set;
        Index {
            authorities: SortedMap::new(),
        }
    }

    pub fn lookup<'a>(&'a self, redactor: &Redactor, req: &Request<'_>) -> Result<Hit<'a>, Miss> {
        let _ = (redactor, req, &self.authorities);
        Err(Miss::Authority)
    }
}

pub(super) fn respond(answer: &Arc<Answer>, tier: Tier, method: &str) -> Response {
    let _ = (answer, tier, method);
    Miss::Overload.response()
}
