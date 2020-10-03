// #![forbid(missing_docs)]
// #![forbid(missing_doc_code_examples)]

//! Memory pools are usefull when allocating and deallocating lots of
//! data of the same size.  
//! Using a memory pool speed up those allocations/deallocations.
//!
//! This crate provides 3 memory pools:
//!
//! ![](https://raw.githubusercontent.com/sebastiencs/shared-arena/images/table.svg)
//!
//! # Performance
//!
//! On my laptop, with Intel i7-6560U, running Clear Linux OS 32700,
//! an allocation with `SharedArena` is 4+ faster than the
//! system allocator:
//!
//! ```ignore
//! Allocation/SharedArena               time:   [25.112 ns 25.678 ns 26.275 ns]
//! Allocation/Box(SystemAllocator)      time:   [112.64 ns 114.44 ns 115.81 ns]
//! ```
//!
//! Performances with more allocations:
//!
//! ![](https://raw.githubusercontent.com/sebastiencs/shared-arena/images/bench.svg)
//!
//! The graphic was generated with criterion, reproducible with `cargo bench`
//!
//! # Implementation details
//!
//! `SharedArena`, `Arena` and `Pool` use the same method of allocation,
//! derived from a [free list](https://en.wikipedia.org/wiki/Free_list).
//!
//! They allocate by pages, which include 63 elements, and keep a list
//! of pages where at least 1 element is not used by the user.  
//! A page has a bitfield of 64 bits, each bit indicates whether or
//! not the element is used.
//!
//! In this bitfield, if the bit is set to zero, the element is
//! already used.
//! So counting the number of trailing zeros gives us the index of an
//! unused element.  
//! Only 1 cpu instruction is necessary to find an unused element:
//! such as `tzcnt`/`bsf` on `x86` and `clz` on `arm`
//!
//! ```ignore
//! [..]1101101000
//! ```
//! With the bitfield above, the 4th element is unused.
//!
//! ![](https://raw.githubusercontent.com/sebastiencs/shared-arena/images/shared_arena.svg)
//!
//! The difference between `SharedArena`/`Arena` and `Pool` is that
//! `Pool` does not use atomics.  
//! Allocating with `Pool` is faster than `SharedArena` and `Arena`.  
//! `Arena` is faster than `SharedArena`
//!
//! # Safety
//!
//! `unsafe` block are used in several places to dereference pointers.  
//! The code is [100% covered](https://codecov.io/gh/sebastiencs/shared-arena/tree/master/src)
//! by the [miri](https://github.com/rust-lang/miri) interpreter,
//! valgrind and 3 sanitizers: address, leak and memory, on each commit.  
//! See the [github actions](https://github.com/sebastiencs/shared-arena/actions)
//!
//! [`SharedArena`]: ./struct.SharedArena.html
//! [`Arena`]: ./struct.Arena.html
//! [`Pool`]: ./struct.Pool.html

mod shared_arena;
mod arena;
mod arena_arc;
mod arena_rc;
mod arena_box;
mod pool;
mod cache_line;
mod common;
mod block;
mod page;

pub use {
    arena::Arena,
    self::shared_arena::SharedArena,
    arena_arc::ArenaArc,
    arena_box::ArenaBox,
    arena_rc::ArenaRc,
    pool::{Pool, PoolBox},
};
