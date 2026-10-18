// Poison-tolerant lock helpers (REFACTOR_PLAN S1.4): a panic while a thread
// holds one of the long-lived app-state mutexes (DB connection, working dir,
// MCP registry, journal, emitter, scheduler/reindex status, process/backup
// registries, handler cache) poisons the lock, and every later
// `.lock().unwrap()` in every command would then panic for the rest of the
// process. These helpers ignore the poison flag and hand out the guard: the
// data behind these locks is either replaced wholesale (Option<Connection>,
// status structs) or is a SQLite connection under WAL + busy_timeout, so
// reusing it after a foreign panic is safe and keeps the app serving commands
// instead of cascading panics.
use std::sync::{Mutex, MutexGuard, RwLock, RwLockReadGuard, RwLockWriteGuard};

pub fn lock_recover<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|e| e.into_inner())
}

pub fn read_recover<T>(lock: &RwLock<T>) -> RwLockReadGuard<'_, T> {
    lock.read().unwrap_or_else(|e| e.into_inner())
}

pub fn write_recover<T>(lock: &RwLock<T>) -> RwLockWriteGuard<'_, T> {
    lock.write().unwrap_or_else(|e| e.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[test]
    fn lock_recover_returns_a_usable_guard_after_poisoning() {
        let mutex = Arc::new(Mutex::new(1u32));
        let holder = Arc::clone(&mutex);
        // The panic happens while the guard is held: the mutex is poisoned.
        let panicker = std::thread::spawn(move || {
            let _guard = holder.lock().unwrap();
            panic!("boom while holding the lock");
        });
        assert!(panicker.join().is_err());
        assert!(mutex.is_poisoned());
        // Plain lock().unwrap() would panic from now on; the helper recovers.
        let mut guard = lock_recover(&mutex);
        assert_eq!(*guard, 1);
        *guard += 1;
        drop(guard);
        assert_eq!(*lock_recover(&mutex), 2);
    }

    #[test]
    fn rwlock_recover_returns_usable_guards_after_poisoning() {
        let lock = Arc::new(RwLock::new(1u32));
        let writer = Arc::clone(&lock);
        let panicker = std::thread::spawn(move || {
            let _guard = writer.write().unwrap();
            panic!("boom while holding the write lock");
        });
        assert!(panicker.join().is_err());
        assert!(lock.is_poisoned());
        assert_eq!(*read_recover(&lock), 1);
        *write_recover(&lock) = 2;
        assert_eq!(*read_recover(&lock), 2);
    }
}
