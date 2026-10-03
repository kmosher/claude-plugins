## Deferred comment judgment

Comment judgment is deferred on this review: a dedicated comment pass (`kmo:finalize`) runs after it. The "Deferred comment judgment" rule applies. Skip heuristics 2 and 11 and do not invoke `comment-writer`; still report wrong or misplaced comments under heuristics 6, 7 and 8. Note the deferral in `meta`.

This overrides the "Where this lens has under-reported" section above for comment prose: do not propose deleting or rewriting a comment for restating the code or for its wording alone.
