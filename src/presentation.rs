use romero::{ProgressEvent, ProgressMoveKind, ProgressRemovalKind};

pub(crate) fn verbose_only(event: &ProgressEvent) -> bool {
    matches!(
        event,
        ProgressEvent::HashSaved { .. }
            | ProgressEvent::CacheCommitted { .. }
            | ProgressEvent::CacheHit { .. }
            | ProgressEvent::Moving {
                kind: ProgressMoveKind::Promotion,
                ..
            }
            | ProgressEvent::WritingCue { .. }
            | ProgressEvent::Removing {
                kind: ProgressRemovalKind::RewrittenCueSource,
                ..
            }
    )
}
