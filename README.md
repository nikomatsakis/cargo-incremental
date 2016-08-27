This is a tool that can be used to fuzz-test rustc's incremental
compilation feature. It will walk your git repository history and
build revisions in succession, checking at each point that an
incremental build performs the same as building from scratch.

Or at least that's the idea. It doesn't really exist yet. :P
