use crate::SearchSort;

/// User-selected browser order. Source groups always retain file-name order.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BrowseSort {
    Random,
    FileName,
    ModifiedDesc,
    ModifiedAsc,
    CreatedDesc,
    CreatedAsc,
}

impl BrowseSort {
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "random" => Some(Self::Random),
            "filename" => Some(Self::FileName),
            "mtime-desc" => Some(Self::ModifiedDesc),
            "mtime-asc" => Some(Self::ModifiedAsc),
            "created-desc" => Some(Self::CreatedDesc),
            "created-asc" => Some(Self::CreatedAsc),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Random => "random",
            Self::FileName => "filename",
            Self::ModifiedDesc => "mtime-desc",
            Self::ModifiedAsc => "mtime-asc",
            Self::CreatedDesc => "created-desc",
            Self::CreatedAsc => "created-asc",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Random => "Random",
            Self::FileName => "File name",
            Self::ModifiedDesc => "Modified: newest first",
            Self::ModifiedAsc => "Modified: oldest first",
            Self::CreatedDesc => "Created: newest first",
            Self::CreatedAsc => "Created: oldest first",
        }
    }

    pub fn search_sort(self, has_source_filter: bool) -> SearchSort {
        if has_source_filter {
            return SearchSort::FileNameAsc;
        }
        match self {
            Self::Random | Self::FileName => SearchSort::FileNameAsc,
            Self::ModifiedDesc => SearchSort::ModifiedTimeDesc,
            Self::ModifiedAsc => SearchSort::ModifiedTimeAsc,
            Self::CreatedDesc => SearchSort::CreatedTimeDesc,
            Self::CreatedAsc => SearchSort::CreatedTimeAsc,
        }
    }

    pub fn is_random(self, has_source_filter: bool) -> bool {
        self == Self::Random && !has_source_filter
    }
}
