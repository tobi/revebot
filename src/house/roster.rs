//! Serialized roster transitions. Guest I/O never runs under this lock.
use super::profile::{BOT_CAP, Profile};
use std::collections::{HashMap, HashSet};

pub type Token = String;
pub enum Slot<T> {
    Creating {
        token: Token,
    },
    Ready(T),
    Replacing {
        token: Token,
        profile: Box<Profile>,
    },
    Deleting {
        token: Token,
        profile: Box<Profile>,
        error: Option<String>,
        retryable: bool,
    },
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum Error {
    #[error("house is closing")]
    Closed,
    #[error("bot cap reached")]
    Capacity,
    #[error("bot is busy creating or deleting")]
    Busy,
    #[error("unknown bot")]
    Unknown,
    #[error("cannot delete the last ready bot")]
    LastBot,
    #[error("stale roster reservation")]
    Stale,
}

pub struct Roster<T> {
    entries: HashMap<String, Slot<T>>,
    // An abandoned guest create may still be finishing. Do not reuse that slug
    // in this process. Restart reclaims the VM before scanning filesystem state.
    retired: HashSet<String>,
    closed: bool,
}
impl<T> Default for Roster<T> {
    fn default() -> Self {
        Self {
            entries: HashMap::new(),
            retired: HashSet::new(),
            closed: false,
        }
    }
}
impl<T> Roster<T> {
    pub fn get(&self, id: &str) -> Option<&Slot<T>> {
        self.entries.get(id)
    }
    pub fn values(&self) -> impl Iterator<Item = &Slot<T>> {
        self.entries.values()
    }
    pub fn iter(&self) -> impl Iterator<Item = (&String, &Slot<T>)> {
        self.entries.iter()
    }
    pub fn ready_mut(&mut self) -> impl Iterator<Item = (&String, &mut T)> {
        self.entries.iter_mut().filter_map(|(id, slot)| match slot {
            Slot::Ready(value) => Some((id, value)),
            _ => None,
        })
    }
    pub fn contains(&self, id: &str) -> bool {
        self.entries.contains_key(id) || self.retired.contains(id)
    }
    pub fn is_closed(&self) -> bool {
        self.closed
    }
    pub fn reserve(&mut self, id: &str) -> Result<Token, Error> {
        if self.closed {
            return Err(Error::Closed);
        }
        if self.entries.len() >= BOT_CAP {
            return Err(Error::Capacity);
        }
        if self.contains(id) {
            return Err(Error::Busy);
        }
        let token = crate::ids::uuid_v7(crate::ids::now_ms());
        self.entries.insert(
            id.into(),
            Slot::Creating {
                token: token.clone(),
            },
        );
        Ok(token)
    }
    pub fn is_reservation(&self, id: &str, token: &str) -> bool {
        !self.closed
            && matches!(self.entries.get(id), Some(Slot::Creating {token:current}) if current == token)
    }
    pub fn publish(&mut self, id: &str, token: &str, value: T) -> Result<(), (Error, T)> {
        if !self.is_reservation(id, token) {
            return Err((Error::Stale, value));
        }
        self.entries.insert(id.into(), Slot::Ready(value));
        Ok(())
    }
    pub fn abandon(&mut self, id: &str, token: &str) -> bool {
        if !matches!(self.entries.get(id), Some(Slot::Creating {token:current}) if current == token)
        {
            return false;
        }
        self.entries.remove(id);
        self.retired.insert(id.into());
        true
    }
    pub fn begin_replace(&mut self, id: &str, profile: Profile) -> Result<(Token, T), Error> {
        if self.closed {
            return Err(Error::Closed);
        }
        if !matches!(self.entries.get(id), Some(Slot::Ready(_))) {
            return Err(if self.entries.contains_key(id) {
                Error::Busy
            } else {
                Error::Unknown
            });
        }
        let token = crate::ids::uuid_v7(crate::ids::now_ms());
        let previous = self.entries.insert(
            id.into(),
            Slot::Replacing {
                token: token.clone(),
                profile: Box::new(profile),
            },
        );
        let Some(Slot::Ready(runtime)) = previous else {
            return Err(Error::Stale);
        };
        Ok((token, runtime))
    }

    pub fn finish_replace(&mut self, id: &str, token: &str, runtime: T) -> Result<(), (Error, T)> {
        if !matches!(self.entries.get(id), Some(Slot::Replacing {token:current,..}) if current == token)
        {
            return Err((Error::Stale, runtime));
        }
        self.entries.insert(id.into(), Slot::Ready(runtime));
        Ok(())
    }

    pub fn cancel_replace(&mut self, id: &str, token: &str, runtime: T) -> Result<(), (Error, T)> {
        self.finish_replace(id, token, runtime)
    }

    pub fn begin_delete(
        &mut self,
        id: &str,
        profile: Profile,
    ) -> Result<(Token, Option<T>), Error> {
        if self.closed {
            return Err(Error::Closed);
        }
        match self.entries.get(id) {
            Some(Slot::Ready(_)) => {
                if self
                    .entries
                    .values()
                    .filter(|s| matches!(s, Slot::Ready(_)))
                    .count()
                    <= 1
                {
                    return Err(Error::LastBot);
                }
            }
            Some(Slot::Deleting {
                error: Some(_),
                retryable: true,
                ..
            }) => {}
            Some(_) => return Err(Error::Busy),
            None => return Err(Error::Unknown),
        }
        let token = crate::ids::uuid_v7(crate::ids::now_ms());
        let old = self.entries.insert(
            id.into(),
            Slot::Deleting {
                token: token.clone(),
                profile: Box::new(profile),
                error: None,
                retryable: false,
            },
        );
        let runtime = match old {
            Some(Slot::Ready(value)) => Some(value),
            _ => None,
        };
        Ok((token, runtime))
    }
    pub fn finish_delete(&mut self, id: &str, token: &str) -> Result<(), Error> {
        if !matches!(self.entries.get(id),Some(Slot::Deleting {token:current,..}) if current==token)
        {
            return Err(Error::Stale);
        }
        self.entries.remove(id);
        Ok(())
    }
    pub fn fail_delete(&mut self, id: &str, token: &str, message: String, retryable: bool) {
        if let Some(Slot::Deleting {
            token: current,
            error,
            retryable: can_retry,
            ..
        }) = self.entries.get_mut(id)
            && current == token
        {
            *error = Some(message);
            *can_retry = retryable;
        }
    }
    pub fn close(&mut self) -> Vec<T> {
        self.closed = true;
        self.entries
            .drain()
            .filter_map(|(_, slot)| match slot {
                Slot::Ready(value) => Some(value),
                Slot::Creating { .. } | Slot::Replacing { .. } | Slot::Deleting { .. } => None,
            })
            .collect()
    }
}

pub fn spawn_job<T, F>(
    jobs: &parking_lot::Mutex<Vec<tokio::task::JoinHandle<()>>>,
    work: F,
) -> tokio::sync::oneshot::Receiver<Result<T, String>>
where
    T: Send + 'static,
    F: std::future::Future<Output = anyhow::Result<T>> + Send + 'static,
{
    let (tx, rx) = tokio::sync::oneshot::channel();
    let mut jobs = jobs.lock();
    jobs.retain(|job| !job.is_finished());
    jobs.push(tokio::spawn(async move {
        let _ = tx.send(work.await.map_err(|e| e.to_string()));
    }));
    rx
}

#[cfg(test)]
mod tests {
    use super::*;
    fn profile(id: &str) -> Profile {
        Profile::parse_for(id, &format!(r#"{{"name":"{id}"}}"#)).unwrap()
    }
    fn ready(roster: &mut Roster<usize>, id: &str) -> Token {
        let token = roster.reserve(id).unwrap();
        roster.publish(id, &token, 1).unwrap();
        token
    }
    #[tokio::test]
    async fn caller_disconnect_does_not_cancel_owned_work() {
        let jobs = parking_lot::Mutex::new(Vec::new());
        let gate = std::sync::Arc::new(tokio::sync::Notify::new());
        let done = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let g = gate.clone();
        let d = done.clone();
        let rx = spawn_job(&jobs, async move {
            g.notified().await;
            d.store(true, std::sync::atomic::Ordering::SeqCst);
            Ok(())
        });
        drop(rx);
        gate.notify_one();
        let handles = std::mem::take(&mut *jobs.lock());
        for handle in handles {
            handle.await.unwrap();
        }
        assert!(done.load(std::sync::atomic::Ordering::SeqCst));
    }

    #[test]
    fn concurrent_deletes_cannot_both_pass_the_last_bot_floor() {
        let mut roster = Roster::default();
        ready(&mut roster, "one");
        ready(&mut roster, "two");
        let (token, _) = roster.begin_delete("one", profile("one")).unwrap();
        assert_eq!(
            roster.begin_delete("two", profile("two")).unwrap_err(),
            Error::LastBot
        );
        roster.finish_delete("one", &token).unwrap();
        assert!(
            roster.reserve("one").is_ok(),
            "successful deletion frees the slug"
        );
    }
    #[test]
    fn stale_guards_and_finishes_cannot_touch_a_new_reservation() {
        let mut roster = Roster::default();
        let old = ready(&mut roster, "one");
        ready(&mut roster, "two");
        let (deletion, _) = roster.begin_delete("one", profile("one")).unwrap();
        roster.finish_delete("one", &deletion).unwrap();
        let new = roster.reserve("one").unwrap();
        assert!(!roster.abandon("one", &old));
        assert!(roster.publish("one", &old, 9).is_err());
        assert!(roster.is_reservation("one", &new));
        roster.publish("one", &new, 2).unwrap();
    }
    #[test]
    fn live_creations_do_not_expire_and_abandoned_slugs_are_retired() {
        let mut roster: Roster<()> = Roster::default();
        let token = roster.reserve("one").unwrap();
        assert!(roster.is_reservation("one", &token));
        assert!(roster.abandon("one", &token));
        assert_eq!(roster.reserve("one").unwrap_err(), Error::Busy);
        assert!(roster.reserve("one-2").is_ok());
    }
    #[test]
    fn failed_deletes_are_explicit_and_only_quiescent_failures_can_retry() {
        let mut roster = Roster::default();
        ready(&mut roster, "one");
        ready(&mut roster, "two");
        let (token, _) = roster.begin_delete("one", profile("one")).unwrap();
        roster.fail_delete("one", &token, "not stopped".into(), false);
        assert_eq!(
            roster.begin_delete("one", profile("one")).unwrap_err(),
            Error::Busy
        );
        roster.fail_delete("one", &token, "VM removal failed".into(), true);
        let (retry, value) = roster.begin_delete("one", profile("one")).unwrap();
        assert!(value.is_none());
        assert_eq!(
            roster.finish_delete("one", &token).unwrap_err(),
            Error::Stale
        );
        roster.finish_delete("one", &retry).unwrap();
    }
    #[test]
    fn creating_and_deleting_slots_count_toward_capacity() {
        let mut roster: Roster<usize> = Roster::default();
        let mut tokens = Vec::new();
        for n in 0..BOT_CAP {
            tokens.push(roster.reserve(&format!("bot-{n}")).unwrap());
        }
        assert_eq!(roster.reserve("extra").unwrap_err(), Error::Capacity);
        roster.publish("bot-0", &tokens[0], 0).unwrap();
        assert_eq!(
            roster.begin_delete("bot-0", profile("bot-0")).unwrap_err(),
            Error::LastBot
        );
        roster.publish("bot-1", &tokens[1], 1).unwrap();
        let (deletion, _) = roster.begin_delete("bot-0", profile("bot-0")).unwrap();
        assert_eq!(roster.reserve("extra").unwrap_err(), Error::Capacity);
        roster.finish_delete("bot-0", &deletion).unwrap();
        assert!(roster.reserve("extra").is_ok());
    }

    #[test]
    fn closing_rejects_publication_and_new_work() {
        let mut roster = Roster::default();
        ready(&mut roster, "one");
        let token = roster.reserve("two").unwrap();
        assert_eq!(roster.close().len(), 1);
        assert_eq!(roster.reserve("three").unwrap_err(), Error::Closed);
        assert!(roster.publish("two", &token, 2).is_err());
    }
}
