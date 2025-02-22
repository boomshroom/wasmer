use super::*;

pub static UNSUPPORTED_JOURNAL: UnsupportedJournal = UnsupportedJournal {};

/// The default for runtime is to use the unsupported journal
/// which will fail to write journal entries if one attempts to do so.
#[derive(Debug, Default)]
pub struct UnsupportedJournal {}

impl ReadableJournal for UnsupportedJournal {
    fn read(&self) -> anyhow::Result<Option<JournalEntry<'_>>> {
        Ok(None)
    }

    fn as_restarted(&self) -> anyhow::Result<Box<DynReadableJournal>> {
        Ok(Box::<UnsupportedJournal>::default())
    }
}

impl WritableJournal for UnsupportedJournal {
    fn write<'a>(&'a self, entry: JournalEntry<'a>) -> anyhow::Result<u64> {
        tracing::debug!("journal event: {:?}", entry);
        Err(anyhow::format_err!("unsupported"))
    }
}

impl Journal for UnsupportedJournal {
    fn split(self) -> (Box<DynWritableJournal>, Box<DynReadableJournal>) {
        (
            Box::<UnsupportedJournal>::default(),
            Box::<UnsupportedJournal>::default(),
        )
    }
}
