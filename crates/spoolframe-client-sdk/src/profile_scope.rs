//! Generations for asynchronous work associated with a foreground profile.
use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};

#[derive(Debug, Clone, Default)]
pub struct ProfileScope(Arc<AtomicU64>);
#[derive(Debug, Clone)]
pub struct ProfileToken {
    scope: ProfileScope,
    generation: u64,
}
impl ProfileScope {
    pub fn generation(&self) -> u64 {
        self.0.load(Ordering::Acquire)
    }
    /// Invalidate successes and errors from previously issued tokens.
    pub fn invalidate(&self) {
        self.0.fetch_add(1, Ordering::AcqRel);
    }
    pub fn token(&self) -> ProfileToken {
        ProfileToken {
            scope: self.clone(),
            generation: self.generation(),
        }
    }
}
impl ProfileToken {
    pub fn is_current(&self) -> bool {
        self.scope.generation() == self.generation
    }
    pub fn accept<T>(&self, result: T) -> Option<T> {
        self.is_current().then_some(result)
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn switching_discards_successes_and_errors() {
        let scope = ProfileScope::default();
        let old = scope.token();
        scope.invalidate();
        let current = scope.token();
        assert!(old.accept(Ok::<_, ()>(1)).is_none());
        assert!(old.accept(Err::<(), _>("expired")).is_none());
        assert_eq!(current.accept(1), Some(1));
    }
}
