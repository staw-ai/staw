//! The bot's local store: one JSON file with each connected person's Staw
//! refresh token, keyed by Slack team id and user id.
//!
//! Written atomically (a temporary file in the same directory, flushed,
//! then renamed over the old one) and readable only by the bot's user
//! (mode 0600 in a 0700 directory it creates). A file found with wider
//! permissions is narrowed at start, with a warning. Access tokens are
//! not kept here: they live an hour, and the bot holds them in memory.

use std::collections::BTreeMap;
use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde::{Deserialize, Serialize};

/// A Slack account: the workspace and the person in it. One token per
/// key, never used for another key's request (rule 3).
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Key {
    pub team: String,
    pub user: String,
}

impl Key {
    pub fn new(team: impl Into<String>, user: impl Into<String>) -> Self {
        Self {
            team: team.into(),
            user: user.into(),
        }
    }

    fn id(&self) -> String {
        format!("{}:{}", self.team, self.user)
    }
}

/// A connected person.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Person {
    /// The Staw refresh token: long-lived and revocable.
    pub refresh_token: String,
    /// The Staw account's name, as shown when connecting.
    pub name: String,
    /// The OAuth client's name; Staw lists the token as
    /// `connector:<client>`.
    pub client: String,
}

#[derive(Default, Serialize, Deserialize)]
struct Document {
    version: u32,
    #[serde(default)]
    people: BTreeMap<String, Person>,
}

/// A store that cannot be read or written.
#[derive(Debug, thiserror::Error)]
#[error("the store at {path}: {message}")]
pub struct Error {
    path: String,
    message: String,
}

pub struct Store {
    path: PathBuf,
    people: Mutex<BTreeMap<String, Person>>,
}

impl Store {
    /// Open the store at `path`, creating its directory when missing. An
    /// absent file is an empty store; a file that is not the store's
    /// JSON is an error, never silently replaced.
    pub fn open(path: &Path) -> Result<Self, Error> {
        let fail = |message: String| Error {
            path: path.display().to_string(),
            message,
        };
        if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty())
            && !dir.exists()
        {
            create_private_dir(dir)
                .map_err(|e| fail(format!("cannot create its directory: {e}")))?;
        }
        let people = match fs::read(path) {
            Ok(bytes) => {
                narrow(path);
                serde_json::from_slice::<Document>(&bytes)
                    .map_err(|e| fail(format!("it is not the bot's store ({e})")))?
                    .people
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => BTreeMap::new(),
            Err(e) => return Err(fail(format!("cannot read it: {e}"))),
        };
        let store = Self {
            path: path.to_path_buf(),
            people: Mutex::new(people),
        };
        // Write once now: an unwritable location fails at start, not on
        // the first person who connects.
        store.save(&store.people.lock().expect("store lock").clone())?;
        Ok(store)
    }

    pub fn get(&self, key: &Key) -> Option<Person> {
        self.people
            .lock()
            .expect("store lock")
            .get(&key.id())
            .cloned()
    }

    pub fn put(&self, key: &Key, person: Person) -> Result<(), Error> {
        let mut people = self.people.lock().expect("store lock");
        let mut next = people.clone();
        next.insert(key.id(), person);
        self.save(&next)?;
        *people = next;
        Ok(())
    }

    /// Forget a person; returns what was stored.
    pub fn remove(&self, key: &Key) -> Result<Option<Person>, Error> {
        let mut people = self.people.lock().expect("store lock");
        let mut next = people.clone();
        let removed = next.remove(&key.id());
        if removed.is_some() {
            self.save(&next)?;
            *people = next;
        }
        Ok(removed)
    }

    fn save(&self, people: &BTreeMap<String, Person>) -> Result<(), Error> {
        let fail = |message: String| Error {
            path: self.path.display().to_string(),
            message,
        };
        let document = Document {
            version: 1,
            people: people.clone(),
        };
        let bytes = serde_json::to_vec_pretty(&document).expect("the store serializes");
        let name = self
            .path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "store".into());
        let temporary = self
            .path
            .with_file_name(format!(".{name}.{}.tmp", std::process::id()));
        let mut file =
            create_private_file(&temporary).map_err(|e| fail(format!("cannot write it: {e}")))?;
        file.write_all(&bytes)
            .and_then(|()| file.sync_all())
            .map_err(|e| fail(format!("cannot write it: {e}")))?;
        drop(file);
        fs::rename(&temporary, &self.path).map_err(|e| {
            let _ = fs::remove_file(&temporary);
            fail(format!("cannot replace it: {e}"))
        })?;
        Ok(())
    }
}

#[cfg(unix)]
fn create_private_file(path: &Path) -> std::io::Result<fs::File> {
    use std::os::unix::fs::OpenOptionsExt as _;
    fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)
}

#[cfg(not(unix))]
fn create_private_file(path: &Path) -> std::io::Result<fs::File> {
    fs::File::create(path)
}

#[cfg(unix)]
fn create_private_dir(dir: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::DirBuilderExt as _;
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
}

#[cfg(not(unix))]
fn create_private_dir(dir: &Path) -> std::io::Result<()> {
    fs::create_dir_all(dir)
}

/// Narrow an existing file to the bot's user.
#[cfg(unix)]
fn narrow(path: &Path) {
    use std::os::unix::fs::PermissionsExt as _;
    if let Ok(meta) = fs::metadata(path)
        && meta.permissions().mode() & 0o077 != 0
    {
        tracing::warn!(
            path = %path.display(),
            "the store was readable by others; narrowing it to the bot's user"
        );
        let _ = fs::set_permissions(path, fs::Permissions::from_mode(0o600));
    }
}

#[cfg(not(unix))]
fn narrow(_path: &Path) {}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch() -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "staw-slack-store-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        dir.join("nested").join("people.json")
    }

    fn alice() -> Person {
        Person {
            refresh_token: "cvg_secret".into(),
            name: "Alice".into(),
            client: "Slack Acme: @alice".into(),
        }
    }

    #[test]
    fn people_survive_a_reopen_and_leave() {
        let path = scratch();
        let key = Key::new("T1", "U1");
        let store = Store::open(&path).unwrap();
        assert_eq!(store.get(&key), None);
        store.put(&key, alice()).unwrap();
        assert_eq!(Store::open(&path).unwrap().get(&key), Some(alice()));
        assert_eq!(store.remove(&key).unwrap(), Some(alice()));
        assert_eq!(Store::open(&path).unwrap().get(&key), None);
        let _ = fs::remove_dir_all(path.parent().unwrap().parent().unwrap());
    }

    #[cfg(unix)]
    #[test]
    fn only_the_bots_user_can_read_it() {
        use std::os::unix::fs::PermissionsExt as _;
        let path = scratch();
        let store = Store::open(&path).unwrap();
        store.put(&Key::new("T1", "U1"), alice()).unwrap();
        let mode = |p: &Path| fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&path), 0o600);
        assert_eq!(mode(path.parent().unwrap()), 0o700);

        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        Store::open(&path).unwrap();
        assert_eq!(mode(&path), 0o600, "a widened file is narrowed at start");
        let _ = fs::remove_dir_all(path.parent().unwrap().parent().unwrap());
    }

    #[test]
    fn a_foreign_file_is_not_replaced() {
        let path = scratch();
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, "not json").unwrap();
        assert!(Store::open(&path).is_err());
        assert_eq!(fs::read_to_string(&path).unwrap(), "not json");
        let _ = fs::remove_dir_all(path.parent().unwrap().parent().unwrap());
    }
}
