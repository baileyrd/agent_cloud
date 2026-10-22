//! Job records (ADR-010): one per job, keyed by a UUIDv5 of `agent/job`, indexed by agent, with
//! the state as the scannable field. Bounded by the gateway's retention, like the agent registry.

use std::path::Path;

use ac_domain::{AgentId, Job, JobId, JobState};
use ac_ports::{JobStore, PortError};
use rusty_multimodal_db_engine::generic::mmap_store::GenericMmapStore;
use rusty_multimodal_db_engine::generic::production::GenericProductionStore;
use rusty_multimodal_db_engine::generic::traits::{
    IndexedField, Record, ScannableField, SchemaTag,
};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::{err, EngineStore, NAMESPACE};

const JOBS_FILE: &str = "jobs.mmap";

#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct JobRecord {
    id: Uuid,
    agent: String,
    job: String,
    state: u32,
    created_ms: u64,
    started_ms: Option<u64>,
    finished_ms: Option<u64>,
    timeout_secs: u32,
    exit_code: Option<i32>,
    key: Option<String>,
    reason: Option<String>,
}

pub(crate) struct ByAgent;
pub(crate) struct ByJobState;

impl Record for JobRecord {
    type Id = Uuid;
    fn id(&self) -> Uuid {
        self.id
    }
}

// On-disk format name: changing the record layout means a new tag and a migration.
impl SchemaTag for JobRecord {
    const SCHEMA_TAG: &'static str = "agent_cloud::JobRecord@1";
}

impl IndexedField<ByAgent> for JobRecord {
    type IndexValue = String;
    fn indexed_value(&self) -> &String {
        &self.agent
    }
}

impl ScannableField<ByJobState> for JobRecord {
    type ScanValue = u32;
    fn scannable_value(&self) -> u32 {
        self.state
    }
    fn set_scannable_value(&mut self, value: u32) {
        self.state = value;
    }
}

pub(crate) type JobTable = GenericProductionStore<GenericMmapStore<JobRecord, ByAgent, ByJobState>>;

pub(crate) fn open(dir: &Path) -> Result<JobTable, PortError> {
    let path = dir.join(JOBS_FILE);
    let core = if path.exists() {
        GenericMmapStore::open_portable(&path)
    } else {
        GenericMmapStore::create(Vec::new(), &path)
    }
    .map_err(|e| err("open job store", e))?;
    Ok(GenericProductionStore::new(core))
}

fn uuid_of(agent: &AgentId, job: &JobId) -> Uuid {
    Uuid::new_v5(&NAMESPACE, format!("job/{agent}/{job}").as_bytes())
}

fn code(state: JobState) -> u32 {
    JobState::ALL
        .iter()
        .position(|s| *s == state)
        .map_or(u32::MAX, |i| i as u32)
}

fn from_code(code: u32) -> Option<JobState> {
    JobState::ALL.get(code as usize).copied()
}

fn record(job: &Job) -> JobRecord {
    JobRecord {
        id: uuid_of(&job.agent, &job.id),
        agent: job.agent.to_string(),
        job: job.id.to_string(),
        state: code(job.state),
        created_ms: job.created_ms,
        started_ms: job.started_ms,
        finished_ms: job.finished_ms,
        timeout_secs: job.timeout_secs,
        exit_code: job.exit_code,
        key: job.key.clone(),
        reason: job.reason.clone(),
    }
}

fn job(rec: JobRecord) -> Result<Job, PortError> {
    Ok(Job {
        agent: AgentId::parse(&rec.agent).map_err(|e| err("decode job", e))?,
        id: JobId::parse(&rec.job).map_err(|e| err("decode job", e))?,
        state: from_code(rec.state)
            .ok_or_else(|| err("decode job", format!("unknown state code {}", rec.state)))?,
        created_ms: rec.created_ms,
        started_ms: rec.started_ms,
        finished_ms: rec.finished_ms,
        timeout_secs: rec.timeout_secs,
        exit_code: rec.exit_code,
        key: rec.key,
        reason: rec.reason,
    })
}

impl JobStore for EngineStore {
    fn put_job(&self, j: &Job) -> Result<(), PortError> {
        let rec = record(j);
        if self.jobs.get::<JobRecord>(rec.id).is_some() {
            return self
                .jobs
                .replace(rec)
                .map_err(|e| err("replace job", format!("{e:?}")));
        }
        self.jobs
            .insert(rec)
            .map_err(|e| err("insert job", format!("{e:?}")))
    }

    fn jobs(&self, agent: &AgentId) -> Result<Vec<Job>, PortError> {
        self.jobs
            .filter_eq::<JobRecord, ByAgent>(&agent.to_string())
            .into_iter()
            .filter_map(|id| self.jobs.get::<JobRecord>(id))
            .filter(|rec| rec.agent == agent.as_str())
            .map(job)
            .collect()
    }

    fn active_jobs(&self) -> Result<Vec<Job>, PortError> {
        let mut out = Vec::new();
        for id in self.jobs.all_ids::<JobRecord>() {
            let Some(rec) = self.jobs.get::<JobRecord>(id) else {
                continue;
            };
            let j = job(rec)?;
            if !j.state.is_terminal() {
                out.push(j);
            }
        }
        Ok(out)
    }

    fn remove_job(&self, agent: &AgentId, id: &JobId) -> Result<(), PortError> {
        let uuid = uuid_of(agent, id);
        if self.jobs.get::<JobRecord>(uuid).is_none() {
            return Ok(());
        }
        self.jobs
            .delete::<JobRecord>(uuid)
            .map_err(|e| err("delete job", format!("{e:?}")))
    }
}

#[cfg(test)]
mod tests {
    use ac_domain::JobEvent;

    use super::*;
    use crate::tests::tmp;

    fn agent(s: &str) -> AgentId {
        AgentId::parse(s).unwrap_or_else(|e| panic!("{e}"))
    }

    fn new_job(a: &str, n: u8) -> Job {
        Job::new(agent(a), JobId::from_random([n; 8]), u64::from(n), 60, None)
    }

    fn open_store(dir: &Path) -> EngineStore {
        EngineStore::open(dir).unwrap_or_else(|e| panic!("{e}"))
    }

    #[test]
    fn put_replace_list_remove_and_reopen() {
        let dir = tmp();
        let mut done = new_job("a1", 2);
        {
            let s = open_store(&dir);
            let mut queued = new_job("a1", 1);
            queued.key = Some("k-1".into());
            assert!(s.put_job(&queued).is_ok());
            assert!(done.apply(JobEvent::Start, 5).is_ok());
            assert!(s.put_job(&done).is_ok());
            assert!(done.apply(JobEvent::Exit(7), 9).is_ok());
            done.reason = Some("exit 7".into());
            assert!(s.put_job(&done).is_ok());
            assert!(s.put_job(&new_job("b2", 3)).is_ok());
        }
        let s = open_store(&dir);
        let mut a1 = s.jobs(&agent("a1")).unwrap_or_else(|e| panic!("{e}"));
        a1.sort_by_key(|j| j.created_ms);
        assert_eq!(a1.len(), 2);
        assert_eq!(a1[0].key.as_deref(), Some("k-1"));
        assert_eq!(a1[1], done);
        let mut active: Vec<String> = s
            .active_jobs()
            .unwrap_or_else(|e| panic!("{e}"))
            .into_iter()
            .map(|j| format!("{}/{}", j.agent, j.state.name()))
            .collect();
        active.sort();
        assert_eq!(active, ["a1/queued", "b2/queued"]);
        assert!(s.remove_job(&agent("a1"), &done.id).is_ok());
        assert!(s.remove_job(&agent("a1"), &done.id).is_ok());
        assert_eq!(s.jobs(&agent("a1")).map(|v| v.len()).ok(), Some(1));
        assert!(matches!(s.jobs(&agent("zz")).as_deref(), Ok([])));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn state_codes_roundtrip() {
        for s in JobState::ALL {
            assert_eq!(from_code(code(s)), Some(s));
        }
        assert_eq!(from_code(99), None);
    }
}
