use crate::grid::{CellPosition, GridSearchScan, GridSearchStatus, GridSource};
use crate::grid_arrow::ArrowGridSource;
use crate::grid_bson::BsonGridSource;
use gpui::SharedString;

#[derive(Debug, Clone)]
pub enum GridContentSource {
    Arrow(ArrowGridSource),
    Bson(BsonGridSource),
}

impl From<ArrowGridSource> for GridContentSource {
    fn from(source: ArrowGridSource) -> Self {
        Self::Arrow(source)
    }
}

impl From<BsonGridSource> for GridContentSource {
    fn from(source: BsonGridSource) -> Self {
        Self::Bson(source)
    }
}

impl GridSource for GridContentSource {
    fn row_count(&self) -> usize {
        match self {
            Self::Arrow(source) => source.row_count(),
            Self::Bson(source) => source.row_count(),
        }
    }

    fn column_count(&self) -> usize {
        match self {
            Self::Arrow(source) => source.column_count(),
            Self::Bson(source) => source.column_count(),
        }
    }

    fn column_name(&self, column: usize) -> Option<&str> {
        match self {
            Self::Arrow(source) => source.column_name(column),
            Self::Bson(source) => source.column_name(column),
        }
    }

    fn column_type(&self, column: usize) -> Option<&str> {
        match self {
            Self::Arrow(source) => source.column_type(column),
            Self::Bson(source) => source.column_type(column),
        }
    }

    fn native_type(&self, column: usize) -> Option<&str> {
        match self {
            Self::Arrow(source) => source.native_type(column),
            Self::Bson(_) => None,
        }
    }

    fn cell_text(&self, position: CellPosition) -> Option<SharedString> {
        match self {
            Self::Arrow(source) => source.cell_text(position),
            Self::Bson(source) => source.cell_text(position),
        }
    }

    fn full_cell_text(&self, position: CellPosition) -> Option<String> {
        match self {
            Self::Arrow(source) => source.full_cell_text(position),
            Self::Bson(source) => source.full_cell_text(position),
        }
    }

    fn cell_is_null(&self, position: CellPosition) -> bool {
        match self {
            Self::Arrow(source) => source.cell_is_null(position),
            Self::Bson(source) => source.cell_is_null(position),
        }
    }

    fn cell_bytes(&self, position: CellPosition) -> Option<&[u8]> {
        match self {
            Self::Arrow(source) => source.cell_bytes(position),
            Self::Bson(_) => None,
        }
    }

    fn cell_str(&self, position: CellPosition) -> Option<&str> {
        match self {
            Self::Arrow(source) => source.cell_str(position),
            Self::Bson(_) => None,
        }
    }

    fn cell_state(&self, position: CellPosition) -> crate::grid::CellState {
        match self {
            Self::Arrow(source) => source.cell_state(position),
            Self::Bson(source) => source.cell_state(position),
        }
    }

    #[cfg(test)]
    fn row_values(&self, row: usize) -> Vec<(String, String)> {
        match self {
            Self::Arrow(source) => source.row_values(row),
            Self::Bson(source) => source.row_values(row),
        }
    }

    fn search_cells(&self, scan: &GridSearchScan<'_>, out: &mut Vec<CellPosition>) -> GridSearchStatus {
        match self {
            Self::Arrow(source) => source.search_cells(scan, out),
            Self::Bson(source) => source.search_cells(scan, out),
        }
    }
}
