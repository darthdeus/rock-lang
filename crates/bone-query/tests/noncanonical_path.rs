//! A non-canonical path spelling (Windows `\\?\` vs `C:\`, or `/./`) must resolve to one module.

use bone_query::BoneWorkspace;
use std::path::Path;
