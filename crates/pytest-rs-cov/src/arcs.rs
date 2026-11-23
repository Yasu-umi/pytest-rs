//! Branch events to line arcs: a port of coverage.py's `BranchArcResolver`
//! (`coverage/bytecode.py`).
//!
//! A branch event is a (source offset, destination offset) pair. Starting at
//! the destination, follow the instructions (through unconditional jumps)
//! until one starts a different source line (the arc), a return (an arc
//! leaving the code object, to `-co_firstlineno`), or another branch (no
//! arc: that branch has its own events). The source line itself and lines
//! of the same multi-line statement do not end the trail, so a compound
//! condition's step to its next test is not an arc, and a loop's false edge
//! reaches the loop header through its backward jump.

use std::collections::{BTreeMap, HashMap};

use pyo3::prelude::*;

/// The opcodes the trail looks at, from this interpreter's `dis` tables.
pub struct Opcodes {
    cache: Option<u8>,
    extended_arg: u8,
    jumps: [bool; 256],
    always_jumps: [bool; 256],
    backward_jumps: [bool; 256],
    returns: [bool; 256],
    no_fall_through: [bool; 256],
}

impl Opcodes {
    pub fn load(py: Python<'_>) -> PyResult<Self> {
        let dis = py.import("dis")?;
        let opmap = dis.getattr("opmap")?;
        let op = |name: &str| -> Option<u8> {
            opmap
                .get_item(name)
                .ok()
                .and_then(|value| value.extract::<u8>().ok())
        };
        let set = |names: &[&str]| {
            let mut table = [false; 256];
            for name in names {
                if let Some(code) = op(name) {
                    table[code as usize] = true;
                }
            }
            table
        };
        let mut jumps = [false; 256];
        for list in ["hasjrel", "hasjabs"] {
            for code in dis.getattr(list)?.try_iter()? {
                let code: usize = code?.extract()?;
                if code < 256 {
                    jumps[code] = true;
                }
            }
        }
        let opname: Vec<String> = dis.getattr("opname")?.extract()?;
        let mut backward_jumps = [false; 256];
        for (code, name) in opname.iter().enumerate().take(256) {
            if jumps[code] && name.contains("JUMP_BACKWARD") {
                backward_jumps[code] = true;
            }
        }
        Ok(Self {
            cache: op("CACHE"),
            extended_arg: op("EXTENDED_ARG").unwrap_or(u8::MAX),
            jumps,
            always_jumps: set(&[
                "JUMP_BACKWARD",
                "JUMP_BACKWARD_NO_INTERRUPT",
                "JUMP_FORWARD",
            ]),
            backward_jumps,
            // RETURN_CONST: 3.13 (coverage.py's resolver only runs on 3.14,
            // which dropped it).
            returns: set(&["RETURN_VALUE", "RETURN_GENERATOR", "RETURN_CONST"]),
            no_fall_through: set(&["RERAISE"]),
        })
    }
}

/// One code object's bytes and line table.
pub struct CodeBytes {
    pub co_code: Vec<u8>,
    pub byte_to_line: HashMap<i64, i64>,
    pub first_line: i64,
}

impl CodeBytes {
    pub fn read(code: &Bound<'_, PyAny>) -> PyResult<Self> {
        let co_code: Vec<u8> = code.getattr("co_code")?.extract()?;
        let mut byte_to_line = HashMap::new();
        for entry in code.call_method0("co_lines")?.try_iter()? {
            let (start, end, line): (i64, i64, Option<i64>) = entry?.extract()?;
            if let Some(line) = line {
                let mut offset = start;
                while offset < end {
                    byte_to_line.insert(offset, line);
                    offset += 2;
                }
            }
        }
        Ok(Self {
            co_code,
            byte_to_line,
            first_line: code.getattr("co_firstlineno")?.extract()?,
        })
    }

    fn line_at(&self, offset: i64, multiline: &BTreeMap<u32, u32>) -> Option<i64> {
        self.byte_to_line
            .get(&offset)
            .map(|&line| Self::demultiline(line, multiline))
    }

    fn demultiline(line: i64, multiline: &BTreeMap<u32, u32>) -> i64 {
        u32::try_from(line)
            .ok()
            .and_then(|l| multiline.get(&l))
            .map_or(line, |&first| i64::from(first))
    }

    /// The (from line, to line) arc of one branch event, or None.
    pub fn resolve(
        &self,
        ops: &Opcodes,
        multiline: &BTreeMap<u32, u32>,
        source: i64,
        dest: i64,
    ) -> Option<(i64, i64)> {
        let from_line = self.line_at(source, multiline)?;
        let code = &self.co_code;
        let max = code.len() as i64;
        let mut offset = dest;
        let mut ext_arg: i64 = 0;
        let mut seen = std::collections::HashSet::new();
        while (0..max).contains(&offset) && seen.insert(offset) {
            let op = code[offset as usize];
            if Some(op) == ops.cache {
                offset += 2;
                continue;
            }
            if op == ops.extended_arg {
                ext_arg = (ext_arg | i64::from(code[offset as usize + 1])) << 8;
                offset += 2;
                continue;
            }
            if let Some(&line) = self.byte_to_line.get(&offset) {
                let line = Self::demultiline(line, multiline);
                if line != 0 && line != from_line {
                    return Some((from_line, line));
                }
            }
            if ops.jumps[op as usize] {
                if ops.always_jumps[op as usize] {
                    let arg = ext_arg | i64::from(code[offset as usize + 1]);
                    // Jump distances count from after the inline CACHE
                    // entries that follow the instruction.
                    let mut next = offset + 2;
                    while next < max && Some(code[next as usize]) == ops.cache {
                        next += 2;
                    }
                    offset = if ops.backward_jumps[op as usize] {
                        next - 2 * arg
                    } else {
                        next + 2 * arg
                    };
                    ext_arg = 0;
                    continue;
                }
                return None;
            }
            if ops.returns[op as usize] {
                return Some((from_line, -self.first_line));
            }
            if ops.no_fall_through[op as usize] {
                return None;
            }
            ext_arg = 0;
            offset += 2;
        }
        None
    }
}
