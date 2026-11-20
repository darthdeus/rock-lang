use super::structure::{Placement, Succ};
use crate::mir::{Block, Inst, MirFunc, Term};
use comfy_la_arena::Idx;

/// An edge into a chain of empty forwarding blocks reaches the chain's end, so
/// an `else if` ladder joins once, not once per rung.
pub fn thread_forwarders(
    func: &MirFunc,
    rpo: &[Idx<Block>],
    forwarder: impl Fn(Idx<Block>) -> bool,
) -> Vec<u32> {
    let n = func.blocks.len();
    let next = |b: Idx<Block>| -> Option<Idx<Block>> {
        match func.blocks[b].term {
            Term::Goto(t) if t != b && forwarder(b) => Some(t),
            _ => None,
        }
    };
    let mut forward: Vec<u32> = (0..n as u32).collect();
    // 0 = unresolved, 1 = on the current chain, 2 = resolved.
    let mut state = vec![0u8; n];
    let mut chain = Vec::new();
    for &b in rpo {
        if state[index(b)] != 0 {
            continue;
        }
        chain.clear();
        let mut cur = b;
        let end = loop {
            match state[index(cur)] {
                2 => break forward[index(cur)],
                // A cycle of forwarders stops at the block that closes it.
                1 => break index(cur) as u32,
                _ => {}
            }
            state[index(cur)] = 1;
            chain.push(cur);
            match next(cur) {
                Some(t) => cur = t,
                None => break index(cur) as u32,
            }
        };
        for &c in &chain {
            state[index(c)] = 2;
            forward[index(c)] = end;
        }
    }
    forward
}

pub fn threaded_successors(func: &MirFunc, forward: &[u32]) -> Succ {
    let n = func.blocks.len();
    let mut succ = Succ {
        offsets: Vec::with_capacity(n + 1),
        targets: Vec::with_capacity(n * 2),
    };
    succ.offsets.push(0);
    for (_, block) in func.blocks.iter() {
        super::successors(&block.term, |target| {
            succ.targets.push(forward[index(target)]);
        });
        succ.offsets.push(succ.targets.len() as u32);
    }
    succ
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Follow {
    Block(Idx<Block>),
    Continue(Idx<Block>),
    End,
}

#[derive(Clone, Copy)]
pub struct NativeLoop {
    pub latch: Idx<Block>,
    pub body: Idx<Block>,
    pub exit: Idx<Block>,
}

pub enum BlockFlow {
    Terminator,
    Next(Idx<Block>),
    Done,
}

/// A follower's scope surrounds the construct preceding it: Lua leaves it
/// implicit and uses a forward goto, JavaScript breaks out of a labeled block.
pub trait PlacementEmit {
    type Error;
    type Ladder;
    type Loop;

    fn thread(&self, block: Idx<Block>) -> Idx<Block> {
        block
    }
    fn native_loop(&self, header: Idx<Block>) -> Option<NativeLoop>;
    fn block(&mut self, block: Idx<Block>) -> Result<BlockFlow, Self::Error>;
    fn terminal(&mut self, block: Idx<Block>, term: &Term) -> Result<(), Self::Error>;
    fn chainable(&self, block: Idx<Block>) -> bool;
    fn condition_effect(&self, cond: Idx<Inst>) -> bool;

    fn ladder(&mut self) -> Self::Ladder;
    fn condition(
        &mut self,
        ladder: &mut Self::Ladder,
        cond: Idx<Inst>,
        negate: bool,
    ) -> Result<(), Self::Error>;
    fn otherwise(&mut self, ladder: &mut Self::Ladder) -> Result<(), Self::Error>;
    fn end_ladder(&mut self, ladder: Self::Ladder) -> Result<(), Self::Error>;

    fn open_follower(&mut self, block: Idx<Block>) -> Result<(), Self::Error>;
    fn close_follower(&mut self, block: Idx<Block>, referenced: bool) -> Result<(), Self::Error>;
    fn follower_jump(&mut self, block: Idx<Block>) -> Result<(), Self::Error>;
    fn begin_loop(
        &mut self,
        header: Idx<Block>,
        native: Option<NativeLoop>,
    ) -> Result<Self::Loop, Self::Error>;
    fn end_loop(
        &mut self,
        header: Idx<Block>,
        native: Option<NativeLoop>,
        token: Self::Loop,
        reaches_end: bool,
        continued: bool,
    ) -> Result<(), Self::Error>;
    fn continue_loop(
        &mut self,
        header: Idx<Block>,
        native: Option<NativeLoop>,
        innermost: bool,
    ) -> Result<(), Self::Error>;
    /// False leaves an outer-loop exit to its follower scope.
    fn break_loop(&mut self, header: Idx<Block>, innermost: bool) -> Result<bool, Self::Error>;
}

#[derive(Clone, Copy)]
enum Inner {
    Node(Idx<Block>),
    Tree(Idx<Block>),
    Loop(Idx<Block>),
}

struct Walk<'a, E> {
    func: &'a MirFunc,
    placement: &'a Placement,
    emit: &'a mut E,
    referenced: Vec<bool>,
    continued: Vec<bool>,
    visited: Vec<bool>,
    reaches_end: bool,
    loops: Vec<Idx<Block>>,
}

fn index(block: Idx<Block>) -> usize {
    block.into_raw().into_u32() as usize
}

impl<E: PlacementEmit> Walk<'_, E> {
    fn falls_to(&mut self, target: Idx<Block>, follow: Follow) -> bool {
        let target = self.emit.thread(target);
        match follow {
            Follow::Block(block) => block == target,
            Follow::Continue(header) => {
                let falls = target == header
                    || self
                        .emit
                        .native_loop(header)
                        .is_some_and(|lp| lp.latch == target);
                self.reaches_end |= falls;
                falls
            }
            Follow::End => false,
        }
    }

    fn tree(&mut self, block: Idx<Block>, follow: Follow) -> Result<(), E::Error> {
        let placement = self.placement;
        let followers = placement.followers(block);
        if placement.is_header(block) {
            let outside: Vec<_> = followers
                .iter()
                .copied()
                .filter(|&f| !placement.in_loop(block, f))
                .collect();
            self.with_followers(&outside, follow, Inner::Loop(block))
        } else {
            self.with_followers(followers, follow, Inner::Node(block))
        }
    }

    fn with_followers(
        &mut self,
        followers: &[Idx<Block>],
        follow: Follow,
        inner: Inner,
    ) -> Result<(), E::Error> {
        match followers.split_last() {
            None => match inner {
                Inner::Node(block) => self.node(block, follow),
                Inner::Tree(block) => self.tree(block, follow),
                Inner::Loop(header) => self.loop_construct(header, follow),
            },
            Some((&last, rest)) => {
                self.emit.open_follower(last)?;
                self.with_followers(rest, Follow::Block(last), inner)?;
                self.emit
                    .close_follower(last, self.referenced[index(last)])?;
                self.tree(last, follow)
            }
        }
    }

    fn loop_construct(&mut self, header: Idx<Block>, after: Follow) -> Result<(), E::Error> {
        let inside: Vec<_> = self
            .placement
            .followers(header)
            .iter()
            .copied()
            .filter(|&f| self.placement.in_loop(header, f))
            .collect();
        self.loops.push(header);
        let outer = std::mem::replace(&mut self.reaches_end, false);
        let native = self.emit.native_loop(header);
        if let Some(lp) = native {
            self.visited[index(header)] = true;
            self.visited[index(lp.latch)] = true;
        }
        let token = self.emit.begin_loop(header, native)?;
        match native {
            Some(lp) if lp.body == lp.latch => {
                debug_assert!(inside.is_empty());
                self.reaches_end = true;
            }
            Some(lp) => {
                self.with_followers(&inside, Follow::Continue(header), Inner::Tree(lp.body))?
            }
            None => self.with_followers(&inside, Follow::Continue(header), Inner::Node(header))?,
        }
        self.emit.end_loop(
            header,
            native,
            token,
            self.reaches_end,
            self.continued[index(header)],
        )?;
        self.reaches_end = outer;
        self.loops.pop();
        if let Some(lp) = native {
            self.branch_to(lp.exit, after)?;
        }
        Ok(())
    }

    fn node(&mut self, block: Idx<Block>, follow: Follow) -> Result<(), E::Error> {
        let seen = std::mem::replace(&mut self.visited[index(block)], true);
        debug_assert!(!seen, "placement prints each block once");
        match self.emit.block(block)? {
            BlockFlow::Done => return Ok(()),
            BlockFlow::Next(target) => return self.branch_to(target, follow),
            BlockFlow::Terminator => {}
        }
        match &self.func.blocks[block].term {
            &Term::Goto(target) => self.branch_to(target, follow),
            &Term::Branch {
                cond,
                then_bb,
                else_bb,
            } => self.emit_if(cond, then_bb, else_bb, follow),
            term => self.emit.terminal(block, term),
        }
    }

    fn chainable(&self, block: Idx<Block>) -> Option<(Idx<Inst>, Idx<Block>, Idx<Block>)> {
        let block = self.emit.thread(block);
        if self.placement.is_follower(block)
            || self.placement.is_header(block)
            || !self.placement.followers(block).is_empty()
            || self.visited[index(block)]
            || !self.emit.chainable(block)
        {
            return None;
        }
        match self.func.blocks[block].term {
            Term::Branch {
                cond,
                then_bb,
                else_bb,
            } => Some((cond, then_bb, else_bb)),
            _ => None,
        }
    }

    fn emit_if(
        &mut self,
        cond: Idx<Inst>,
        then_bb: Idx<Block>,
        else_bb: Idx<Block>,
        follow: Follow,
    ) -> Result<(), E::Error> {
        let then_empty = self.falls_to(then_bb, follow);
        let else_empty = self.falls_to(else_bb, follow);
        let mut ladder = self.emit.ladder();
        if then_empty && else_empty {
            if self.emit.condition_effect(cond) {
                self.emit.condition(&mut ladder, cond, false)?;
                self.emit.end_ladder(ladder)?;
            }
            return Ok(());
        }
        self.emit.condition(&mut ladder, cond, then_empty)?;
        self.branch_to(if then_empty { else_bb } else { then_bb }, follow)?;
        if !then_empty {
            let mut remaining = else_bb;
            while !self.falls_to(remaining, follow) {
                if let Some((cond, yes, no)) = self.chainable(remaining)
                    && !self.falls_to(yes, follow)
                {
                    self.visited[index(self.emit.thread(remaining))] = true;
                    self.emit.condition(&mut ladder, cond, false)?;
                    self.branch_to(yes, follow)?;
                    remaining = no;
                    continue;
                }
                self.emit.otherwise(&mut ladder)?;
                self.branch_to(remaining, follow)?;
                break;
            }
        }
        self.emit.end_ladder(ladder)
    }

    fn branch_to(&mut self, target: Idx<Block>, follow: Follow) -> Result<(), E::Error> {
        let target = self.emit.thread(target);
        if self.falls_to(target, follow) {
            return Ok(());
        }
        for i in (0..self.loops.len()).rev() {
            let header = self.loops[i];
            let native = self.emit.native_loop(header);
            let innermost = i + 1 == self.loops.len();
            if native.is_some_and(|lp| self.emit.thread(lp.exit) == target)
                && self.emit.break_loop(header, innermost)?
            {
                return Ok(());
            }
            if target == header || native.is_some_and(|lp| lp.latch == target) {
                self.continued[index(header)] = true;
                return self.emit.continue_loop(header, native, innermost);
            }
        }
        if self.placement.is_follower(target) {
            self.referenced[index(target)] = true;
            self.emit.follower_jump(target)
        } else {
            self.tree(target, follow)
        }
    }
}

pub fn emit<E: PlacementEmit>(
    func: &MirFunc,
    placement: &Placement,
    entry: Idx<Block>,
    emit: &mut E,
) -> Result<(), E::Error> {
    let n = func.blocks.len();
    Walk {
        func,
        placement,
        emit,
        referenced: vec![false; n],
        continued: vec![false; n],
        visited: vec![false; n],
        reaches_end: false,
        loops: Vec::new(),
    }
    .tree(entry, Follow::End)
}
