# ZOS Service Rules (zos-service.md)

These rules apply to **all ZOS request handlers and async storage state machines**
(write, mkdir, delete, rename, chmod, etc.).

Violations are **bugs**, not style issues.

---

## 0. Declared Safety Invariants (MANDATORY)

Every ZOS module **MUST** declare its safety properties at the top of the file.

At minimum, the module **MUST** state:
- What *success* means
- What partial failure is acceptable (if any)
- What is explicitly forbidden

### Example
- Success means: *both inode and content are committed*
- Acceptable: orphan content
- Forbidden: inode pointing to missing content

If invariants are not documented, the code is incomplete.

---

## 1. Input Parsing Rules

- ❌ Never treat JSON parse errors as path errors
- ✅ JSON parse failure → `InvalidRequest`
- ✅ Error responses must include parse failure context
- ❌ Never proceed after parse failure

---

## 2. Path Validation Rules

- ✅ `validate_path()` is the **single source of truth** for path correctness
- ❌ No ad-hoc path validation outside `validate_path()`
- ❌ No implicit normalization unless explicitly documented

### Write-specific rules
- ❌ Writing to `/` is forbidden
- ❌ Trailing slash (`/a/b/`) is forbidden
- ❌ Empty basename is forbidden

### Mkdir-specific rules
- ✅ Trailing slash policy must be explicit:
  - Either normalized (`/a/b/` → `/a/b`)
  - Or rejected consistently
- ❌ Mixed behavior is forbidden

---

## 3. Canonicalization & Keying Rules

The following functions **MUST agree on canonical form**:
- `validate_path`
- `parent_path`
- `inode_key`
- `content_key`

### Forbidden
- `/a/b` and `/a/b/` mapping to different keys
- Parent path not matching stored inode key
- Root (`/`) behaving inconsistently

Any mismatch is a **correctness bug**.

---

## 4. Permission & Authorization Rules (FAIL-CLOSED)

Permission checks are **security-critical**.

- ✅ `PermissionContext` must come from a trusted source
- ❌ Never trust user-provided identity data
- ❌ Never continue on permission uncertainty

### Parent-dependent operations MUST:
1. Read parent inode
2. Require `READ_OK`
3. Parse inode successfully
4. Verify inode type (directory)
5. Check permissions
6. Proceed only if all succeed

### Mandatory Fail-Closed Conditions
If **any** of the following fail → **DENY**:
- Parent inode missing
- Parent inode corrupt or unparsable
- Parent inode wrong type
- Permission evaluation fails
- Permission check returns false

❌ “Continue anyway” is a **security vulnerability**

---

## 5. Storage Result Handling Rules (STRICT)

Every storage call **MUST** enumerate expected result types.

### Exists
Allowed:
- `EXISTS_OK`
- `NOT_FOUND` (if documented)

Everything else → `StorageError`

### Read
Allowed:
- `READ_OK`
- `NOT_FOUND` (explicitly handled)

Everything else → `StorageError`

### Write
Allowed:
- `WRITE_OK`

Everything else → `StorageError`

❌ Silent fallthrough is forbidden  
❌ Treating unexpected results as success is forbidden

---

## 6. State Machine Rules (PendingOp)

- ❌ No overloaded or misleading `PendingOp` variants
- ❌ No sentinel values (`pid = 0`, `tag = 0`)
- ✅ Each async operation must have:
  - A dedicated `PendingOp`
  - An explicit stage enum
- ✅ Each stage must have a single responsibility

### Responses
- ❌ Intermediate stages must never respond
- ✅ Exactly one response per client request
- ✅ Only the final stage may respond

---

## 7. Write Operation Rules (MANDATORY ORDER)

Write operations **MUST** follow this order:

1. Read parent inode
2. Validate parent + permissions
3. Write content
4. Write inode
5. Respond success

### Forbidden States
- ❌ Inode written before content
- ❌ Success response before inode commit
- ❌ Inode pointing to missing content

### Acceptable State
- ✅ Orphan content (must be acknowledged and GC planned)

---

## 8. Mkdir Rules

Mkdir must have **parity with write**.

Required stages:
1. Exists check
2. Parent read
3. Parent permission check
4. Inode write
5. Respond

- ❌ Skipping parent permission check is forbidden
- ❌ Ignoring `create_parents` is forbidden
  - Either implement it
  - Or return `NotSupported`

---

## 9. Error Reporting Rules

- ✅ Errors must include:
  - Operation
  - Result type
  - Human-readable result name
- ❌ Generic `"Write failed"` errors are forbidden
- ❌ Leaking content or secrets in errors is forbidden

---

## 10. Logging Rules

- ✅ Log operation start and completion
- ✅ Log permission denials explicitly
- ✅ Log unexpected storage results
- ❌ Never log file contents
- ❌ Never log sensitive paths without redaction

---

## 11. Resource & DoS Rules

- ✅ Enforce content size limits
- ✅ Enforce pending-op limits per client
- ✅ Pending ops must be bounded or expirable
- ❌ Unbounded async state is forbidden

---

## 12. Deprecated Code Rules

- Deprecated handlers:
  - Must be explicitly labeled
  - Must forward to the new implementation
  - Must have a removal plan
- ❌ Deprecated paths may not introduce weaker semantics

---

## 13. Testing Requirements (REQUIRED)

Every operation **MUST** have tests for:
- Path edge cases
- Permission denial
- Corrupt parent inode
- Parent not directory
- Unexpected storage result types
- Partial failure scenarios
- Success invariants

If it isn’t tested, it isn’t correct.

---

## 14. Review Gate

Any PR touching ZOS code **MUST** answer:

1. What is the success invariant?
2. What partial failure is allowed?
3. Where is permission enforced?
4. What happens on unexpected storage results?
5. Can this code fail open?

If any answer is unclear → **reject the PR**.