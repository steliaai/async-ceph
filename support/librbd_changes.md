since librbd versions aren't tagged, here are the hashes for various librbd version bump commits:

- 1.15 -> 1.17: `git diff dc885574d99371c5869f276cf3f8a9ddb6f2967e..8010dfa23e7f275d40b94a9f4ed9fbe601e091d3 -- src/include/rbd/`
- 1.17 -> 1.18: `git diff 8010dfa23e7f275d40b94a9f4ed9fbe601e091d3..2204f7b55a361755902222df61af242abac459e5  -- src/include/rbd/`
- 1.18 -> 1.19: `git diff 2204f7b55a361755902222df61af242abac459e5..61c7b30bbd1f90ee9a1ee506f3a7b0908e0c4de7  -- src/include/rbd/`
- 1.19 -> 1.20: `git diff 61c7b30bbd1f90ee9a1ee506f3a7b0908e0c4de7..9fa558750c80242a7f6d89d3b422e168aa9079cf  -- src/include/rbd/`

to find these commits use `git log -L 34,+3:src/include/rbd/librbd.h`.
