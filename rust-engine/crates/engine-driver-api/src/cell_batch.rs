/// `CCellText::flags` bit 0: the cell is SQL NULL (its text is the display
/// word `NULL`, which a real text value can equal).
pub const CELL_FLAG_NULL: u32 = 1;

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct CCellText {
    pub row: u64,
    pub column: u64,
    pub offset: u64,
    pub length: u64,
    pub flags: u32,
}

#[repr(C)]
#[derive(Debug)]
pub struct CCellTextBatch {
    pub cells: *mut CCellText,
    pub cell_count: u64,
    pub text_buffer: *mut u8,
    pub text_buffer_len: u64,
}
