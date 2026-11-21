use std::fmt;

use crate::mir::MirFunc;

impl fmt::Display for MirFunc {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "fn {}({:?}) {{", self.name, self.params)?;
        for (id, block) in self.blocks.iter() {
            write!(f, "\n  {id:?}:")?;
            for &inst in &block.insts {
                write!(f, "\n    {inst:?}: {:?}", self.insts[inst])?;
            }
            write!(f, "\n    {:?}", block.term)?;
        }
        write!(f, "\n}}")
    }
}
