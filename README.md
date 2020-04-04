<h1 align="center">shared-arena</h1>
<div align="center">
  <strong>
    A thread-safe, lock-free & efficient memory pool
  </strong>
</div>


<br />

<div align="center">
  <a href="https://github.com/sebastiencs/shared-arena">
    <img src="https://img.shields.io/github/last-commit/sebastiencs/shared-arena?style=flat-square"
         alt="Last activity" />
  </a>
  <!-- Status -->
  <a href="https://github.com/sebastiencs/shared-arena">
    <img src="https://img.shields.io/badge/status-stable-orange?style=flat-square"
         alt="Status" />
  </a>
  <!-- Rust toolchain -->
  <a href="https://github.com/sebastiencs/shared-arena">
    <img src="https://img.shields.io/badge/rust-stable-blue?style=flat-square"
         alt="rust toolchain" />
  </a>
</div>

<br />

Memory pools are usefull when allocating and deallocating lots of data of the same size.  
Using a memory pool speed up those allocations/deallocations.  

This crate provides 2 memory pools:
- `SharedArena`: For multi-threaded usage
- `Pool`: For single thread only


![](https://github.com/sebastiencs/shared-arena/blob/images/table.svg)

# Performance

On my laptop, with Intel i7-6560U, running Clear Linux OS 32700, An allocation with `SharedArena` is 4+ faster than the
system allocator:

```
Allocation/SharedArena               time:   [25.112 ns 25.678 ns 26.275 ns]
Allocation/Box(SystemAllocator)      time:   [112.64 ns 114.44 ns 115.81 ns]
```

Performances with more allocations:

![](https://github.com/sebastiencs/shared-arena/blob/images/bench.svg)

The graphic was generated with criterion, reproducible with `cargo bench`

# Implementation details

`SharedArena` and `Pool` use the same method of allocation, derived from a [Free list](https://en.wikipedia.org/wiki/Free_list).  

They allocate by pages, which include 63 elements, and keep a list of pages where at least 1 element is not used by the user.  
A page has a bitfield of 64 bits, each bit indicates whether or not the element is used. The 64th bit is reserved for the arena itself.  

In this bitfield, if the bit is set to zero, the element is already used.  
So counting the number of trailing zeros gives us the index of the next free element.  
Only 1 cpu instruction is necessary to find an empty space: such as `tzcnt`/`bsf` on `x86` and `clz` on `arm`

```
[..]1101101000
```
With the bitfield above, the 4th element is free.  

The difference between `SharedArena` and `Pool` is that `SharedArena` uses atomics.

![](https://github.com/sebastiencs/shared-arena/blob/images/shared_arena.svg)
