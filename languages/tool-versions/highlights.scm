(comment) @comment

; Tool name (first token on the line) — highlighted like a key, matching the
; `[tools]` keys in mise.toml.
(entry tool: (word) @property)

; Version specifier(s).
(entry version: (word) @string)
