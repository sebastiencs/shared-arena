// #![forbid(missing_docs)]
// #![forbid(missing_doc_code_examples)]

//! Memory pools are usefull when allocating and deallocating lots of data of the same size.  
//! Using a memory pool speed up those allocations/deallocations.  
//!
//! This crate provides 3 memory pools:
//! - [`SharedArena`]
//! - [`Arena`]
//! - [`Pool`]
//!
//! For more details visit shared-arena's repository:  
//! [https://github.com/sebastiencs/shared-arena](https://github.com/sebastiencs/shared-arena)
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
