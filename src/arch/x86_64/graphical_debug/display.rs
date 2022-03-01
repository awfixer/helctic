use core::{cmp, ptr, slice};

use super::FONT;

/// A display
pub struct Display {
    pub width: usize,
    pub height: usize,
    pub data: &'static mut [u32],
}

impl Display {
    pub fn new(width: usize, height: usize, data_ptr: *mut u32) -> Display {
        let size = width * height;
        let data = unsafe {
            ptr::write_bytes(data_ptr, 0, size);
            slice::from_raw_parts_mut(data_ptr, size)
        };
        Display {
            width,
            height,
            data
        }
    }

    /// Draw a character
    pub fn char(&mut self, x: usize, y: usize, character: char, color: u32) {
        if x + 8 <= self.width && y + 16 <= self.height {
            let mut dst = self.data.as_mut_ptr() as usize + (y * self.width + x) * 4;

            let font_i = 16 * (character as usize);
            if font_i + 16 <= FONT.len() {
                for row in 0..16 {
                    let row_data = FONT[font_i + row];
                    for col in 0..8 {
                        if (row_data >> (7 - col)) & 1 == 1 {
                            unsafe { *((dst + col * 4) as *mut u32)  = color; }
                        }
                    }
                    dst += self.width * 4;
                }
            }
        }
    }

    // Scroll the screen
    pub fn scroll(&mut self, lines: usize) {
        let offset = cmp::min(self.height, lines) * self.width;
        let size = self.data.len() - offset;
        unsafe {
            let ptr = self.data.as_mut_ptr();
            ptr::copy(ptr.add(offset), ptr, size);
            ptr::write_bytes(ptr.add(size), 0, offset);
        }
    }
}
