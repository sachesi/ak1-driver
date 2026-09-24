//! In-memory dialog templates. Dialog units follow the font and the system
//! DPI, so the layout scales without a resource compiler.

use windows::Win32::UI::WindowsAndMessaging::{DLGTEMPLATE, WS_CHILD, WS_VISIBLE};

pub enum Class {
    Button,
    Static,
    ComboBox,
    Named(&'static str),
}

pub struct Template {
    words: Vec<u16>,
    items: u16,
}

/// Index of `cdit` in DLGTEMPLATE, counted in 16-bit words.
const ITEM_COUNT_WORD: usize = 4;

impl Template {
    pub fn new(title: &str, style: u32, font: &str, point_size: u16, [cx, cy]: [i16; 2]) -> Template {
        let mut template = Template { words: Vec::new(), items: 0 };
        template.dword(style);
        template.dword(0);
        template.words.push(0);
        template.shorts([0, 0, cx, cy]);
        // No menu, the default dialog class.
        template.words.extend([0, 0]);
        template.string(title);
        template.words.push(point_size);
        template.string(font);
        template
    }

    pub fn item(&mut self, class: Class, text: &str, id: u16, style: u32, rect: [i16; 4]) {
        if self.words.len() % 2 == 1 {
            self.words.push(0);
        }
        self.dword(WS_CHILD.0 | WS_VISIBLE.0 | style);
        self.dword(0);
        self.shorts(rect);
        self.words.push(id);
        match class {
            Class::Button => self.words.extend([0xffff, 0x0080]),
            Class::Static => self.words.extend([0xffff, 0x0082]),
            Class::ComboBox => self.words.extend([0xffff, 0x0085]),
            Class::Named(name) => self.string(name),
        }
        self.string(text);
        // No creation data.
        self.words.push(0);
        self.items += 1;
        self.words[ITEM_COUNT_WORD] = self.items;
    }

    /// The template in a DWORD-aligned buffer, as DialogBoxIndirectParamW requires.
    pub fn build(&self) -> Vec<u32> {
        self.words.chunks(2).map(|pair| u32::from(pair[0]) | u32::from(*pair.get(1).unwrap_or(&0)) << 16).collect()
    }

    pub fn as_template(buffer: &[u32]) -> *const DLGTEMPLATE {
        buffer.as_ptr().cast()
    }

    fn dword(&mut self, value: u32) {
        self.words.extend([value as u16, (value >> 16) as u16]);
    }

    fn shorts(&mut self, values: [i16; 4]) {
        self.words.extend(values.map(|v| v as u16));
    }

    fn string(&mut self, text: &str) {
        self.words.extend(text.encode_utf16().chain([0]));
    }
}
