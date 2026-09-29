//! The 16 field types of an XDAG block (4-bit codes stored in the header).

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum FieldType {
    /// Unused / padding field (also used as free nonce space).
    Nonce = 0x0,
    Head = 0x1,
    /// Link to a block, spending from the block balance (legacy model).
    In = 0x2,
    /// Link to a block; the amount must be zero since xdagj 0.7.
    Out = 0x3,
    SignIn = 0x4,
    SignOut = 0x5,
    PublicKey0 = 0x6,
    PublicKey1 = 0x7,
    HeadTest = 0x8,
    Remark = 0x9,
    Snapshot = 0xA,
    /// Miner address of a main block (an address output with amount 0).
    Coinbase = 0xB,
    /// Account input (address + amount).
    Input = 0xC,
    /// Account output (address + amount).
    Output = 0xD,
    TxNonce = 0xE,
    /// Reserved in xdagj; carries the payload root of a Nova extension block.
    Extension = 0xF,
}

impl FieldType {
    pub fn from_nibble(n: u8) -> FieldType {
        match n & 0xf {
            0x0 => FieldType::Nonce,
            0x1 => FieldType::Head,
            0x2 => FieldType::In,
            0x3 => FieldType::Out,
            0x4 => FieldType::SignIn,
            0x5 => FieldType::SignOut,
            0x6 => FieldType::PublicKey0,
            0x7 => FieldType::PublicKey1,
            0x8 => FieldType::HeadTest,
            0x9 => FieldType::Remark,
            0xA => FieldType::Snapshot,
            0xB => FieldType::Coinbase,
            0xC => FieldType::Input,
            0xD => FieldType::Output,
            0xE => FieldType::TxNonce,
            _ => FieldType::Extension,
        }
    }

    pub fn nibble(self) -> u8 {
        self as u8
    }

    pub fn is_sign(self) -> bool {
        matches!(self, FieldType::SignIn | FieldType::SignOut)
    }
}

/// Nibble `i` of the header type word is the type of field `i`.
pub fn field_type_at(type_word: u64, i: usize) -> FieldType {
    FieldType::from_nibble(((type_word >> (i * 4)) & 0xf) as u8)
}

pub fn set_field_type(type_word: &mut u64, i: usize, t: FieldType) {
    *type_word &= !(0xfu64 << (i * 4));
    *type_word |= (t.nibble() as u64) << (i * 4);
}
