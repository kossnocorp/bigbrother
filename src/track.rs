use crate::prelude::*;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TrackEvent {
    InitialTracked(TrackEventFile),
    Tracked(TrackEventFile),
    Untracked(TrackEventFile),
    Created(TrackEventFile),
    Changed(TrackEventFile),
    Removed(TrackEventFile),
    Moved(TrackEventFileMove),
    Error(TrackEventError),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrackEventFile {
    pub path: AbsPath,
    #[cfg(feature = "source-filter")]
    pub lang: Option<SourceLang>,
}

impl TrackEventFile {
    pub fn from_path(path: &AbsPath) -> Self {
        // let path = to_relative(cwd, abs_path);
        #[cfg(feature = "source-filter")]
        let lang = path.source_lang();
        Self {
            path: path.clone(),
            #[cfg(feature = "source-filter")]
            lang,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrackEventFileMove {
    pub from: AbsPath,
    pub to: AbsPath,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrackEventError {
    pub message: String,
}
