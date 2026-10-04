; Upstream captures Elixir definitions as `@local.definition.var`, `.parameter`,
; `.function`, `.type` and `.import`, which the highlighter does not read. Their
; patterns nest `(_ ...)` twenty levels deep under every `binary_operator` and
; `call`, which made highlighting quadratic in the length of an operator chain
; or a run of nested calls, so only the captures the highlighter reads remain.

; References
(identifier) @local.reference

(alias) @local.reference

; Local Function Scopes
(call
  target: ((identifier) @_identifier
    (#any-of? @_identifier "def" "defp" "defmacro" "defmacrop" "defguard" "defguardp" "defn" "defnp" "for"))
  (arguments)
  (#set! definition.function.scope parent)
  (do_block)?) @local.scope

; ExUnit Test Scopes
(call
  target: ((identifier) @_identifier
    (#eq? @_identifier "test"))
  (arguments
    (string))
  (do_block)?) @local.scope

; Stab Clause Scopes
(stab_clause) @local.scope
