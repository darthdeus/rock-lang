//! Structured printing of one body: `if`/`elseif`/`else` and counted `for`
//! loops in place of a label per block and a `goto` per edge.
//!
//! The walk follows [`skull_mir::cfg::structure::place`]: each block is
//! printed inline at its one forward edge, or as a follower after the
//! construct of its placement parent. An edge to a follower is a
//! fallthrough when the follower is the next code, a `break` when it is the
//! exit of the innermost `for`, and otherwise a forward `goto` to a label at
//! the follower. Every local but a `for` loop's is declared at the top of the
//! body, so no such jump enters the scope of one.
//!
//! Any other loop stays a label at its header and a backward `goto` per
//! back edge: LuaJIT starts recording at the instruction a backward `goto`
//! compiles to, and a `while true do` header instead records from the top,
//! which aborts several times more traces while they warm up. An edge back
//! to a `for` header from deeper than its last statement is a `goto` to a
//! label ending the loop body.
//!
//! One pass prints the body: every `goto` to a follower or to the end of a
//! `for` body is forward, so whether its label is needed is known by the
//! time the label would be printed.

use super::*;
use crate::frame_limits::ControlLines;
use skull_mir::cfg::structure::{Placement, Succ, place};

#[inline]
fn bx(b: Idx<Block>) -> usize {
    raw(b) as usize
}

const NONE: u32 = u32::MAX;

pub(crate) struct Shape {
    placement: Placement,
    /// Block -> the block an edge into it reaches once empty forwarding
    /// blocks are skipped.
    forward: Vec<u32>,
    /// Counted-loop header -> its latch, which the `for` line subsumes.
    latch_of: Vec<u32>,
}

impl Shape {
    /// `None` when the body cannot be structured (an irreducible CFG, a
    /// `yield`, or a counted loop whose natural loop is not its recognized
    /// region); the caller prints it flat.
    /// Only the blocks in `rpo` (those reachable from the entry) are read: a
    /// hoisted lambda's arena holds every block of its enclosing function.
    pub(crate) fn analyze(
        func: &MirFunc,
        entry: Idx<Block>,
        rpo: &[Idx<Block>],
        counted: &HashMap<Idx<Block>, CountedLoop>,
        forwarder: impl Fn(Idx<Block>) -> bool,
    ) -> Option<Shape> {
        let n = func.blocks.len();
        let forward = thread_forwarders(func, rpo, counted, forwarder);
        // An unreachable block's edges are listed unthreaded; `place` never
        // reads them.
        let mut succ = Succ {
            offsets: Vec::with_capacity(n + 1),
            targets: Vec::with_capacity(n * 2),
        };
        succ.offsets.push(0);
        for (_, blk) in func.blocks.iter() {
            if matches!(blk.term, Term::Yield { .. }) {
                return None;
            }
            successors(&blk.term, |s| succ.targets.push(forward[bx(s)]));
            succ.offsets.push(succ.targets.len() as u32);
        }
        let mut latch_of = vec![NONE; n];
        let mut unplaced: Vec<Idx<Block>> = Vec::with_capacity(counted.len());
        for (&h, cl) in counted {
            latch_of[bx(h)] = raw(cl.latch);
            unplaced.push(cl.latch);
        }
        let placement = place(entry, &succ, &unplaced)?;
        // The `for` subsumes header and latch, so its natural loop must lie in
        // the recognized region (threaded forwarders may drop out of it): it
        // holds header, latch and nothing outside the region.
        for (&h, cl) in counted {
            let mut latches = placement.latches(h);
            if latches.next() != Some(cl.latch) || latches.next().is_some() {
                return None;
            }
            let inside = cl
                .region
                .iter()
                .filter(|&&b| placement.in_loop(h, b))
                .count();
            if placement.loop_size(h) != inside + 2 {
                return None;
            }
        }
        Some(Shape {
            placement,
            forward,
            latch_of,
        })
    }

    fn thread(&self, t: Idx<Block>) -> Idx<Block> {
        Idx::from_raw(comfy_la_arena::RawIdx::from_u32(self.forward[bx(t)]))
    }

    fn counted_latch(&self, h: Idx<Block>) -> Option<Idx<Block>> {
        let l = self.latch_of[bx(h)];
        (l != NONE).then(|| Idx::from_raw(comfy_la_arena::RawIdx::from_u32(l)))
    }
}

/// Where each block's edges really lead: an edge into a chain of empty
/// forwarding blocks reaches the chain's end, so the chain never becomes a
/// merge of its own (an `else if` ladder joins once, not once per rung).
fn thread_forwarders(
    func: &MirFunc,
    rpo: &[Idx<Block>],
    counted: &HashMap<Idx<Block>, CountedLoop>,
    forwarder: impl Fn(Idx<Block>) -> bool,
) -> Vec<u32> {
    let n = func.blocks.len();
    let next = |b: Idx<Block>| -> Option<Idx<Block>> {
        match func.blocks[b].term {
            Term::Goto(t) if t != b && !counted.contains_key(&b) && forwarder(b) => Some(t),
            _ => None,
        }
    };
    let mut forward: Vec<u32> = (0..n as u32).collect();
    // 0 = unresolved, 1 = on the current chain, 2 = resolved.
    let mut state = vec![0u8; n];
    let mut chain = Vec::new();
    for &b in rpo {
        if state[bx(b)] != 0 {
            continue;
        }
        chain.clear();
        let mut cur = b;
        let end = loop {
            match state[bx(cur)] {
                2 => break forward[bx(cur)],
                // A cycle of forwarders: stop at the block that closes it.
                1 => break raw(cur),
                _ => {}
            }
            state[bx(cur)] = 1;
            chain.push(cur);
            match next(cur) {
                Some(t) => cur = t,
                None => break raw(cur),
            }
        };
        for &c in &chain {
            state[bx(c)] = 2;
            forward[bx(c)] = end;
        }
    }
    forward
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Follow {
    /// The code after this construct is block `b`.
    Block(Idx<Block>),
    /// The code after this construct is the end of loop `h`'s body.
    Cont(Idx<Block>),
    /// Nothing follows: the function ends.
    End,
}

#[derive(Clone, Copy)]
enum Inner {
    Node(Idx<Block>),
    Tree(Idx<Block>),
    Loop(Idx<Block>),
}

struct Walk<'w, 'e> {
    em: &'w mut Emitter<'e>,
    sh: &'w Shape,
    out: &'w mut String,
    /// Followers some edge reaches by `goto`, and loops some edge continues
    /// by `goto`; each is set before its label is printed.
    goto_ref: Vec<bool>,
    cont_ref: Vec<bool>,
    visited: Vec<bool>,
    /// Whether some edge falls through to the end of the innermost loop's
    /// body.
    reaches_end: std::cell::Cell<bool>,
    /// Open loops, innermost last, with where each one's `break`s start in
    /// `breaks`.
    loops: Vec<(Idx<Block>, usize)>,
    /// What the jump-length model needs.
    control: ControlLines,
    breaks: Vec<u32>,
    escapes: Vec<u32>,
    /// `(line, key, jump instruction)` of each `goto` and `(key, line)` of
    /// each label; see [`key`].
    gotos: Vec<(u32, u32, u8)>,
    labels: Vec<(u32, u32)>,
}

/// The label kinds a structured body prints, each named by its block.
#[derive(Clone, Copy)]
enum Label {
    /// `L<block>`: a follower that some edge reaches by `goto`.
    Follower = 0,
    /// `c<block>`: the end of a `for` loop's body.
    Continue = 1,
    /// `w<block>`: the header of a loop other than a `for`.
    Header = 2,
}

impl Label {
    fn prefix(self) -> &'static str {
        match self {
            Label::Follower => "L",
            Label::Continue => "c",
            Label::Header => "w",
        }
    }
}

fn key(kind: Label, b: Idx<Block>) -> u32 {
    4 * raw(b) + kind as u32
}

/// An open `if`/`elseif` ladder: the conditional jump still waiting for the
/// next rung, and where its rungs' escape jumps start in `escapes`.
struct Ladder {
    first: bool,
    pending: Option<(u32, u8)>,
    escapes: usize,
}

impl Walk<'_, '_> {
    /// Write a control line that compiles to `instructions` jumps or loop
    /// instructions at least; returns its index in `control`.
    fn line(&mut self, s: &str, instructions: u8) -> u32 {
        let at = self.control.line(self.out.len(), instructions);
        self.out.push(' ');
        self.out.push_str(s);
        self.out.push('\n');
        at
    }

    fn jump(&mut self, from: (u32, u8), to: (u32, u8)) {
        self.control.jumps.push((from.0, from.1, to.0, to.1));
    }

    /// `::<kind><block>::`.
    fn label(&mut self, kind: Label, b: Idx<Block>) {
        let at = self.control.line(self.out.len(), 0);
        self.control.names += 1;
        self.labels.push((key(kind, b), at));
        self.out.push_str(" ::");
        self.out.push_str(kind.prefix());
        push_uint(self.out, self.em.func.emit_block_id(b));
        self.out.push_str("::\n");
    }

    /// `goto <kind><block>`. Only a jump back to a loop header is preceded by
    /// the `LOOP` instruction LuaJIT counts hotness with.
    fn goto(&mut self, kind: Label, b: Idx<Block>) {
        match kind {
            Label::Follower => self.goto_ref[bx(b)] = true,
            Label::Continue => self.cont_ref[bx(b)] = true,
            Label::Header => {}
        }
        let jump = u8::from(matches!(kind, Label::Header));
        let at = self.control.line(self.out.len(), jump + 1);
        self.control.names += 1;
        self.gotos.push((at, key(kind, b), jump));
        self.out.push_str(" goto ");
        self.out.push_str(kind.prefix());
        push_uint(self.out, self.em.func.emit_block_id(b));
        self.out.push('\n');
    }

    fn ladder(&self) -> Ladder {
        Ladder {
            first: true,
            pending: None,
            escapes: self.escapes.len(),
        }
    }

    /// The next rung's `if`/`elseif` line, which compiles to an escape jump
    /// out of the rung above (an `elseif`), then the test of `cond` and its
    /// conditional jump. A test of a constant folds away; any other is a
    /// compare or truth test followed by the jump.
    fn rung(&mut self, lad: &mut Ladder, cond: Idx<Inst>) {
        let escape = u8::from(!lad.first);
        let test = if matches!(self.em.func.insts[cond], Inst::Const(_)) {
            0
        } else {
            2
        };
        let at = self.control.line(self.out.len(), escape + test);
        if !lad.first {
            self.escapes.push(at);
        }
        if let Some(p) = lad.pending.take() {
            self.jump(p, (at, escape));
        }
        lad.first = false;
        lad.pending = (test > 0).then_some((at, escape + 1));
    }

    fn ladder_else(&mut self, lad: &mut Ladder) {
        let at = self.line("else", 1);
        self.escapes.push(at);
        if let Some(p) = lad.pending.take() {
            self.jump(p, (at, 1));
        }
    }

    fn ladder_end(&mut self, lad: Ladder) {
        let at = self.line("end", 0);
        if let Some(p) = lad.pending {
            self.jump(p, (at, 0));
        }
        for i in lad.escapes..self.escapes.len() {
            let from = self.escapes[i];
            self.jump((from, 0), (at, 0));
        }
        self.escapes.truncate(lad.escapes);
    }

    fn falls_to(&self, t: Idx<Block>, follow: Follow) -> bool {
        let t = self.sh.thread(t);
        match follow {
            Follow::Block(b) => b == t,
            Follow::Cont(h) => {
                let reaches = t == h || self.sh.counted_latch(h) == Some(t);
                if reaches {
                    self.reaches_end.set(true);
                }
                reaches
            }
            Follow::End => false,
        }
    }

    fn tree(&mut self, x: Idx<Block>, follow: Follow) {
        let sh = self.sh;
        let fs = sh.placement.followers(x);
        if sh.placement.is_header(x) {
            // Followers inside the loop are placed inside its body.
            let outside: Vec<Idx<Block>> = fs
                .iter()
                .copied()
                .filter(|f| !sh.placement.in_loop(x, *f))
                .collect();
            self.with_followers(&outside, follow, Inner::Loop(x));
        } else {
            self.with_followers(fs, follow, Inner::Node(x));
        }
    }

    fn with_followers(&mut self, fs: &[Idx<Block>], follow: Follow, inner: Inner) {
        match fs.split_last() {
            None => match inner {
                Inner::Node(x) => self.node(x, follow),
                Inner::Tree(x) => self.tree(x, follow),
                Inner::Loop(h) => self.loop_construct(h, follow),
            },
            Some((&last, rest)) => {
                self.with_followers(rest, Follow::Block(last), inner);
                if self.goto_ref[bx(last)] {
                    self.label(Label::Follower, last);
                }
                self.tree(last, follow);
            }
        }
    }

    fn loop_construct(&mut self, h: Idx<Block>, after: Follow) {
        let sh = self.sh;
        let inside: Vec<Idx<Block>> = sh
            .placement
            .followers(h)
            .iter()
            .copied()
            .filter(|f| sh.placement.in_loop(h, *f))
            .collect();
        self.loops.push((h, self.breaks.len()));
        // Over-approximated (a fall-through test can pass for an edge printed
        // another way), which at worst leaves a dead `goto`.
        let outer = self.reaches_end.replace(false);
        let end = if let Some(latch) = sh.counted_latch(h) {
            self.visited[bx(h)] = true;
            self.visited[bx(latch)] = true;
            let body_entry = self.em.loops[&h].body_entry;
            let em = &*self.em;
            // Dead on entry to the header is dead for the whole loop unless
            // redefined, so the header's kills go once, above it.
            em.write_kills(self.out, &em.kills.at_entry[bx(h)]);
            let head = self.control.line(self.out.len(), 1);
            em.write_for_head(self.out, h);
            self.with_followers(&inside, Follow::Cont(h), Inner::Tree(body_entry));
            if self.cont_ref[bx(h)] {
                self.label(Label::Continue, h);
            }
            let em = &*self.em;
            em.write_kills(self.out, &em.kills.at_entry[bx(latch)]);
            // FORI skips past FORL; FORL loops back into the body.
            let end = self.line("end", 1);
            self.jump((head, 0), (end, 1));
            self.jump((end, 0), (head, 1));
            end
        } else {
            self.label(Label::Header, h);
            self.with_followers(&inside, Follow::Cont(h), Inner::Node(h));
            if self.reaches_end.get() {
                self.goto(Label::Header, h);
            }
            0
        };
        self.reaches_end.set(outer);
        let (_, first) = self.loops.pop().expect("open loop");
        for i in first..self.breaks.len() {
            let from = self.breaks[i];
            self.jump((from, 0), (end, 1));
        }
        self.breaks.truncate(first);
        if sh.counted_latch(h).is_some() {
            // A `for` falls out of its `end` to its exit, which need not be
            // the code printed next.
            let exit = self.em.loops[&h].exit;
            self.branch_to(exit, after);
        }
    }

    fn node(&mut self, x: Idx<Block>, follow: Follow) {
        let seen = std::mem::replace(&mut self.visited[bx(x)], true);
        debug_assert!(!seen, "placement prints each block once");
        if self.em.emit_block_statements(self.out, x) {
            return;
        }
        let func = self.em.func;
        match &func.blocks[x].term {
            Term::Return(_) | Term::Unreachable => {
                self.em.emit_term(self.out, &func.blocks[x].term, x);
            }
            &Term::Goto(t) => self.branch_to(t, follow),
            &Term::Branch {
                cond,
                then_bb,
                else_bb,
            } => {
                if let Some(next) = self.em.selection_next(x, cond, then_bb, else_bb) {
                    self.em.emit_term(self.out, &func.blocks[x].term, x);
                    if let Some(join) = next {
                        self.branch_to(join, follow);
                    }
                    return;
                }
                self.emit_if(cond, then_bb, else_bb, follow);
            }
            Term::Switch {
                discr,
                cases,
                default,
            } => {
                if cases.is_empty() {
                    self.branch_to(*default, follow);
                    return;
                }
                let mut lad = self.ladder();
                for (i, (lit, b)) in cases.iter().enumerate() {
                    if i == 0 {
                        self.em.mark_inline_operand(self.out, *discr);
                    }
                    self.rung(&mut lad, *discr);
                    self.out.push_str(if i == 0 { " if " } else { " elseif " });
                    self.em.write_opnd(self.out, *discr);
                    self.out.push_str(" == ");
                    write_lit(self.out, lit);
                    self.out.push_str(" then\n");
                    self.branch_to(*b, follow);
                }
                if !self.falls_to(*default, follow) {
                    self.ladder_else(&mut lad);
                    self.branch_to(*default, follow);
                }
                self.ladder_end(lad);
            }
            Term::Yield { .. } => unreachable!("a body that yields prints flat"),
        }
    }

    /// `b` can continue an `elseif` ladder: it prints nothing of its own and
    /// ends in a plain two-way branch.
    fn chainable(&self, b: Idx<Block>) -> Option<(Idx<Inst>, Idx<Block>, Idx<Block>)> {
        let b = self.sh.thread(b);
        let p = &self.sh.placement;
        if p.is_follower(b)
            || p.is_header(b)
            || !p.followers(b).is_empty()
            || self.visited[bx(b)]
            || self.em.tail_forwards[bx(b)].is_some()
            || !self.em.kills.at_entry[bx(b)].is_empty()
        {
            return None;
        }
        let Term::Branch {
            cond,
            then_bb,
            else_bb,
        } = self.em.func.blocks[b].term
        else {
            return None;
        };
        if self.em.func.blocks[b]
            .insts
            .iter()
            .any(|&id| self.em.emits_statement(id) || self.em.suppressed[raw(id) as usize])
        {
            return None;
        }
        if self.em.selection_next(b, cond, then_bb, else_bb).is_some() {
            return None;
        }
        Some((cond, then_bb, else_bb))
    }

    /// An `if`/`elseif` line on `cond`, opening the next rung of `lad`.
    fn write_cond(&mut self, lad: &mut Ladder, keyword: &str, cond: Idx<Inst>, negate: bool) {
        self.em.mark_inline_operand(self.out, cond);
        self.rung(lad, cond);
        self.out.push(' ');
        self.out.push_str(keyword);
        // Operands render as bare names or parenthesized expressions, so a
        // prefix `not` binds to the whole condition.
        self.out.push_str(if negate { " not " } else { " " });
        self.em.write_opnd(self.out, cond);
        self.out.push_str(" then\n");
    }

    fn emit_if(
        &mut self,
        cond: Idx<Inst>,
        then_bb: Idx<Block>,
        else_bb: Idx<Block>,
        follow: Follow,
    ) {
        let then_empty = self.falls_to(then_bb, follow);
        let else_empty = self.falls_to(else_bb, follow);
        if then_empty && else_empty {
            return;
        }
        let mut lad = self.ladder();
        if then_empty {
            self.write_cond(&mut lad, "if", cond, true);
            self.branch_to(else_bb, follow);
            self.ladder_end(lad);
            return;
        }
        self.write_cond(&mut lad, "if", cond, false);
        self.branch_to(then_bb, follow);
        let mut e = else_bb;
        while !self.falls_to(e, follow) {
            if let Some((c, t, next)) = self.chainable(e)
                && !self.falls_to(t, follow)
            {
                let rung = self.sh.thread(e);
                self.visited[bx(rung)] = true;
                self.write_cond(&mut lad, "elseif", c, false);
                self.branch_to(t, follow);
                e = next;
                continue;
            }
            self.ladder_else(&mut lad);
            self.branch_to(e, follow);
            break;
        }
        self.ladder_end(lad);
    }

    fn branch_to(&mut self, t: Idx<Block>, follow: Follow) {
        let t = self.sh.thread(t);
        if self.falls_to(t, follow) {
            return;
        }
        let depth = self.loops.len();
        for i in (0..depth).rev() {
            let (h, _) = self.loops[i];
            let counted = self.sh.counted_latch(h).is_some();
            // A `break` lands where the `for` falls out to its exit.
            if i + 1 == depth && counted && self.sh.thread(self.em.loops[&h].exit) == t {
                let at = self.line("break", 1);
                self.control.names += 1;
                self.breaks.push(at);
                return;
            }
            if t == h || self.sh.counted_latch(h) == Some(t) {
                let kind = if counted {
                    Label::Continue
                } else {
                    Label::Header
                };
                self.goto(kind, h);
                return;
            }
        }
        if self.sh.placement.is_follower(t) {
            self.goto(Label::Follower, t);
            return;
        }
        self.tree(t, follow);
    }
}

/// Print the structured body of `em`'s function from `entry` into `out`,
/// returning its control lines for the jump-length model.
pub(crate) fn emit_body(
    em: &mut Emitter<'_>,
    shape: &Shape,
    entry: Idx<Block>,
    out: &mut String,
) -> ControlLines {
    let n = em.func.blocks.len();
    em.structured = true;
    let mut walk = Walk {
        em,
        sh: shape,
        out,
        goto_ref: vec![false; n],
        cont_ref: vec![false; n],
        visited: vec![false; n],
        reaches_end: std::cell::Cell::new(false),
        loops: Vec::new(),
        control: ControlLines::default(),
        breaks: Vec::new(),
        escapes: Vec::new(),
        gotos: Vec::new(),
        labels: Vec::new(),
    };
    walk.tree(entry, Follow::End);
    let Walk {
        mut control,
        gotos,
        mut labels,
        ..
    } = walk;
    labels.sort_unstable();
    for (from, key, jump) in gotos {
        if let Ok(i) = labels.binary_search_by_key(&key, |&(k, _)| k) {
            control.jumps.push((from, jump, labels[i].1, 0));
        }
    }
    control
}
