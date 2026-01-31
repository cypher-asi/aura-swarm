# Create Zero App

Create a new Zero OS application with the following name and description:

- **App Name**: `{app_name}` (snake_case)
- **Display Name**: `{AppName}` (PascalCase)  
- **Description**: {app_description}

---

## Architecture Overview

Zero OS apps follow a split architecture:
- **Backend**: Rust/WASM process running in the kernel
- **Frontend**: React/TypeScript UI running in the browser
- **Protocol**: Binary IPC protocol connecting backend ↔ frontend

```
┌─────────────────┐      IPC Protocol      ┌─────────────────┐
│  Rust Backend   │ ◄──────────────────────► │  React Frontend │
│  (WASM Process) │   MSG_APP_STATE (0x2000) │  (ZUI Components)│
│                 │   MSG_APP_INPUT (0x2001) │                 │
└─────────────────┘                          └─────────────────┘
```

---

## Files to Create/Modify

### Backend (Rust/WASM) - 5 Files

#### 1. App Binary: `crates/zos-apps/src/bin/{app_name}.rs`

```rust
//! {AppName} Application
//!
//! {app_description}. Demonstrates:
//! - [List key features]

#![cfg_attr(target_arch = "wasm32", no_main)]

extern crate alloc;

use alloc::format;
use alloc::string::String;
use zos_apps::app_protocol::{tags, {AppName}State};
use zos_apps::manifest::{APP_NAME}_MANIFEST;
use zos_apps::syscall;
use zos_apps::{app_main, AppContext, AppError, AppManifest, ControlFlow, Message, ZeroApp};

/// {AppName} application state
#[derive(Default)]
pub struct {AppName}App {
    // Add your state fields here
}

impl {AppName}App {
    /// Send current state to UI
    fn send_state(&self, ctx: &AppContext) -> Result<(), AppError> {
        let state = {AppName}State {
            // Populate state fields
        };

        let bytes = state.to_bytes();

        if let Some(slot) = ctx.ui_endpoint {
            syscall::send(slot, tags::MSG_APP_STATE, &bytes)
                .map_err(|e| AppError::IpcError(format!("Send failed: {}", e)))?;
        }

        Ok(())
    }
}

impl ZeroApp for {AppName}App {
    fn manifest() -> &'static AppManifest {
        &{APP_NAME}_MANIFEST
    }

    fn init(&mut self, ctx: &AppContext) -> Result<(), AppError> {
        // Initialize state
        self.send_state(ctx)
    }

    fn update(&mut self, _ctx: &AppContext) -> ControlFlow {
        // Periodic updates (called at ~60fps)
        ControlFlow::Yield
    }

    fn on_message(&mut self, ctx: &AppContext, msg: Message) -> Result<(), AppError> {
        use zos_apps::app_protocol::{tags, InputEvent};
        
        if msg.tag == tags::MSG_APP_INPUT {
            let event = InputEvent::from_bytes(&msg.data)?;
            
            if let Some(name) = event.button_name() {
                // Handle button press by name
                match name {
                    // Add button handlers
                    _ => {}
                }
                self.send_state(ctx)?;
            }
        }

        Ok(())
    }

    fn shutdown(&mut self, _ctx: &AppContext) {
        syscall::debug("{AppName}: shutting down");
    }
}

// Entry point
app_main!({AppName}App);

#[cfg(not(target_arch = "wasm32"))]
fn main() {
    println!("{AppName} app is meant to run as WASM in Zero OS");
}
```

#### 2. App Manifest: Update `crates/zos-apps/src/manifest.rs`

Add the manifest constant:

```rust
/// {AppName} app manifest
pub static {APP_NAME}_MANIFEST: AppManifest = AppManifest {
    id: "com.zero.{app_name}",
    name: "{AppName}",
    version: "1.0.0",
    description: "{app_description}",
    capabilities: &[CapabilityRequest {
        object_type: ObjectType::Endpoint,
        permissions: Permissions::read_write(),
        reason: "Send state updates to display",
        required: true,
    }],
};
```

#### 3. App Protocol State: `crates/zos-apps/src/app_protocol/{app_name}.rs`

```rust
//! {AppName} State Protocol
//!
//! Serialization for {AppName} app state.

use super::type_tags::TYPE_{APP_NAME}_STATE;
use super::wire::{decode_string, decode_u8, encode_string, Envelope};
use crate::error::ProtocolError;
use alloc::string::String;
use alloc::vec::Vec;

/// {AppName} app state - sent via MSG_APP_STATE
#[derive(Clone, Debug, Default)]
pub struct {AppName}State {
    // Add your state fields here
    // Example:
    // pub display: String,
    // pub is_active: bool,
}

impl {AppName}State {
    /// Create a new {AppName}State
    pub fn new(/* parameters */) -> Self {
        Self {
            // Initialize fields
        }
    }

    /// Serialize to bytes (for sending via IPC)
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut payload = Vec::new();

        // Type tag
        payload.push(TYPE_{APP_NAME}_STATE);

        // Encode each field using wire helpers:
        // - Strings: payload.extend_from_slice(&encode_string(&self.field));
        // - Bools: payload.push(if self.field { 1 } else { 0 });
        // - u8: payload.push(self.field);
        // - u32: payload.extend_from_slice(&self.field.to_le_bytes());

        // Wrap in envelope
        let envelope = Envelope::new(TYPE_{APP_NAME}_STATE, payload);
        super::wire::encode_envelope(&envelope)
    }

    /// Deserialize from bytes (received via IPC)
    pub fn from_bytes(data: &[u8]) -> Result<Self, ProtocolError> {
        let envelope = super::wire::decode_envelope(data)?;

        if envelope.type_tag != TYPE_{APP_NAME}_STATE {
            return Err(ProtocolError::UnexpectedType {
                expected: TYPE_{APP_NAME}_STATE,
                got: envelope.type_tag,
            });
        }

        let payload = &envelope.payload;
        if payload.is_empty() {
            return Err(ProtocolError::EmptyPayload);
        }

        // Skip type tag in payload
        let mut cursor = 1;

        // Decode each field:
        // - Strings: let field = decode_string(payload, &mut cursor)?;
        // - Bools: let field = decode_u8(payload, &mut cursor)? != 0;
        // - u8: let field = decode_u8(payload, &mut cursor)?;

        Ok({AppName}State {
            // Assign decoded fields
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_{app_name}_state_roundtrip() {
        let state = {AppName}State {
            // Set test values
        };

        let bytes = state.to_bytes();
        let decoded = {AppName}State::from_bytes(&bytes).unwrap();

        // Assert fields match
    }
}
```

#### 4. Protocol Module Exports: Update `crates/zos-apps/src/app_protocol/mod.rs`

Add to the module:

```rust
// Add module declaration
mod {app_name};

// Add to pub use exports
pub use {app_name}::{AppName}State;
```

Add to `type_tags` module:

```rust
pub const TYPE_{APP_NAME}_STATE: u8 = {type_tag_value}; // Use next available (0x03, 0x04, etc.)
```

#### 5. Cargo.toml Binary: Update `crates/zos-apps/Cargo.toml`

Add binary declaration:

```toml
[[bin]]
name = "{app_name}"
path = "src/bin/{app_name}.rs"
```

---

### Frontend (React/TypeScript) - 4 Files

## ZUI COMPONENT REQUIREMENTS (CRITICAL)

**All Zero OS apps MUST use ZUI components from `@cypher-asi/zui` for UI consistency.**

### Available ZUI Components

| Component | Usage |
|-----------|-------|
| `Panel` | Primary container - supports `variant="glass"`, `variant="default"`, `border="none"` |
| `Button` | All buttons - supports `variant="primary"`, `"secondary"`, `"ghost"`, `size="lg"` |
| `Text` | All text display - supports `size="lg"`, `"sm"`, `"xs"`, `variant="muted"`, `as="div"` |
| `Label` | Status indicators, tags - supports `size="xs"`, `variant="success"`, `"warning"` |
| `Card` / `CardItem` | List items, grouped content |
| `Menu` / `MenuItem` | Dropdown menus, context menus |
| `Drawer` | Side panels, collapsible sections |
| `GroupCollapsible` | Expandable content sections |
| `PageEmptyState` | Placeholder for unimplemented features |

### ZUI Usage Rules

1. **NEVER use raw HTML elements** (`<div>`, `<button>`, `<span>`) when a ZUI component exists
2. **Use `Panel` instead of `<div>`** for all containers and layout wrappers
3. **Use `Button` instead of `<button>`** for all interactive buttons
4. **Use `Text` instead of `<p>`, `<span>`, `<h1>`** for all text content
5. **Use `Label` instead of `<span>`** for status indicators, badges, tags
6. **Glass effect**: Use `<Panel variant="glass">` for frosted glass backgrounds
7. **Icons**: Import from `lucide-react` and wrap in Panel if needed

### Anti-Patterns to AVOID

- Creating custom styled `<div>` elements when `Panel` would work
- Using inline styles for colors/fonts (use ZUI's built-in variants)
- Building custom button styles (use Button variants: primary, secondary, ghost)
- Hardcoding colors instead of CSS variables (`--color-accent`, `--color-border`, etc.)
- Using raw HTML for text (`<p>`, `<span>`) instead of `Text` or `Label`

---

#### 1. App Component: `web/apps/{AppName}App/{AppName}App.tsx`

```tsx
import { useState, useEffect, useCallback } from 'react';
import { Panel, Button, Text, Label } from '@cypher-asi/zui';
// Import icons from lucide-react as needed:
// import { IconName } from 'lucide-react';
import { decode{AppName}State, {AppName}State } from '../shared/app-protocol';
import styles from './{AppName}App.module.css';

/**
 * {AppName} App - {app_description}
 *
 * Uses ZUI components: Panel, Button, Text, Label
 */
export function {AppName}App() {
  const [state, setState] = useState<{AppName}State>({
    // Initial state matching TypeScript interface
  });

  // Handle incoming state from WASM backend
  const handleMessage = useCallback((data: Uint8Array) => {
    const decoded = decode{AppName}State(data);
    if (decoded) setState(decoded);
  }, []);

  // Register message handler for WASM communication
  useEffect(() => {
    (window as unknown as { {app_name}AppHandler?: (data: Uint8Array) => void }).{app_name}AppHandler = handleMessage;
    return () => {
      delete (window as unknown as { {app_name}AppHandler?: (data: Uint8Array) => void }).{app_name}AppHandler;
    };
  }, [handleMessage]);

  // Handle button clicks - send to backend
  const handleButton = useCallback((buttonName: string) => {
    // For now, handle locally. When connected to WASM:
    // sendInputEvent({ type: 'button', name: buttonName });
    console.log('Button pressed:', buttonName);
  }, []);

  return (
    <Panel className={styles.container}>
      {/* Main content panel with glass effect */}
      <Panel variant="glass" className={styles.mainPanel}>
        
        {/* Example: Title section */}
        <Text as="div" size="lg" className={styles.title}>
          {AppName}
        </Text>

        {/* Example: Status indicators */}
        <Panel className={styles.statusRow}>
          <Label size="xs" variant="success">Active</Label>
        </Panel>

        {/* Example: Button grid */}
        <Panel border="none" className={styles.buttonGrid}>
          <Button variant="ghost" size="lg" onClick={() => handleButton('action_1')}>
            Action 1
          </Button>
          <Button variant="primary" size="lg" onClick={() => handleButton('submit')}>
            Submit
          </Button>
        </Panel>

      </Panel>
    </Panel>
  );
}
```

#### 2. CSS Module: `web/apps/{AppName}App/{AppName}App.module.css`

```css
/* Container - centers the app content */
.container {
  display: flex;
  align-items: center;
  justify-content: center;
  height: 100%;
  background: transparent !important;
  border: none !important;
}

/* Main panel with glass effect */
.mainPanel {
  width: 100%;
  max-width: 400px;
  display: flex;
  flex-direction: column;
  gap: 16px;
  padding: 24px !important;
}

/* Title styling */
.title {
  font-size: 24px !important;
  font-weight: 500 !important;
  text-align: center;
}

/* Status row */
.statusRow {
  display: flex;
  gap: 8px;
  justify-content: center;
  background: transparent !important;
  border: none !important;
}

/* Button grid - customize columns as needed */
.buttonGrid {
  display: grid;
  grid-template-columns: repeat(2, 1fr);
  gap: 8px;
  background: transparent;
  padding: 0;
}

/* Optional: Icon container */
.iconPanel {
  width: 64px;
  height: 64px;
  border-radius: 50% !important;
  display: flex;
  align-items: center;
  justify-content: center;
  background: var(--color-accent-subtle, rgba(1, 244, 203, 0.1)) !important;
  border: none !important;
}

.icon {
  color: var(--color-accent, #01f4cb);
}

/* Monospace display (for data/numbers) */
.monoDisplay {
  font-family: var(--font-mono, 'Monaco', 'Menlo', monospace) !important;
  letter-spacing: 1px;
}
```

#### 3. Protocol Decoder: Update `web/apps/shared/app-protocol.ts`

Add the type tag constant (in the constants section):

```typescript
export const TYPE_{APP_NAME}_STATE = {type_tag_value}; // Match Rust value
```

Add the TypeScript interface:

```typescript
// ============================================================================
// {AppName} State
// ============================================================================

export interface {AppName}State {
  // Define fields matching Rust struct
  // Example:
  // display: string;
  // isActive: boolean;
}

/**
 * Decode {AppName}State from bytes (received via IPC)
 */
export function decode{AppName}State(data: Uint8Array): {AppName}State | null {
  const envelope = decodeEnvelope(data);
  if (!envelope) return null;

  if (envelope.typeTag !== TYPE_{APP_NAME}_STATE) {
    console.error(
      `Expected {APP_NAME}_STATE (${TYPE_{APP_NAME}_STATE}), got ${envelope.typeTag}`
    );
    return null;
  }

  const payload = envelope.payload;
  if (payload.length === 0) {
    return null;
  }

  // Skip type tag in payload (byte 0)
  const cursor = { pos: 1 };

  // Decode each field using helpers:
  // - Strings: const field = decodeString(payload, cursor);
  // - Bools: const field = decodeU8(payload, cursor) !== 0;
  // - u8: const field = decodeU8(payload, cursor);
  // - Optional char: const field = decodeOptionalChar(payload, cursor);

  // Check for null on each decode
  // if (field === null) return null;

  return {
    // Assign decoded fields (use camelCase for TypeScript)
  };
}
```

#### 4. App Router: Update `web/apps/AppRouter/AppRouter.tsx`

Add import:

```typescript
import { {AppName}App } from '../{AppName}App/{AppName}App';
```

Add case in switch statement:

```typescript
case '{app_name}':
case 'com.zero.{app_name}':
  return <{AppName}App />;
```

---

### Build System - 2 Files

#### 1. Update `build.ps1`

Add to the copy section in `Build-Processes` function:

```powershell
Copy-Item "$releaseDir\{app_name}.wasm" "$ProjectRoot\web\processes\" -Force
```

#### 2. Update `Makefile`

Add to the `build-processes` target:

```makefile
cp target/wasm32-unknown-unknown/release/{app_name}.wasm web/processes/
```

---

## Wire Protocol Reference

### Envelope Format

```
┌─────────┬──────────┬─────────────┬─────────────────────┐
│ version │ type_tag │ payload_len │       payload       │
│  (u8)   │   (u8)   │    (u16)    │      (bytes)        │
└─────────┴──────────┴─────────────┴─────────────────────┘
   1 byte    1 byte     2 bytes      0-65535 bytes
```

### Message Tags

| Tag | Value | Direction | Purpose |
|-----|-------|-----------|---------|
| `MSG_APP_STATE` | `0x2000` | Backend → UI | State update |
| `MSG_APP_INPUT` | `0x2001` | UI → Backend | User input event |
| `MSG_UI_READY` | `0x2002` | UI → Backend | UI surface ready |
| `MSG_APP_FOCUS` | `0x2003` | Backend → UI | Request focus |
| `MSG_APP_ERROR` | `0x2004` | Backend → UI | Error notification |

### Type Tags (State)

| Tag | Value | App |
|-----|-------|-----|
| `TYPE_CLOCK_STATE` | `0x01` | Clock |
| `TYPE_CALCULATOR_STATE` | `0x02` | Calculator |
| `TYPE_{APP_NAME}_STATE` | `{type_tag_value}` | {AppName} |

### Input Event Types

| Type | Value | Payload |
|------|-------|---------|
| `TYPE_BUTTON_PRESS` | `0x10` | Button name (string) |
| `TYPE_TEXT_INPUT` | `0x11` | Input text (string) |
| `TYPE_KEY_PRESS` | `0x12` | Key code (u32) + modifiers (u8) |
| `TYPE_FOCUS_CHANGE` | `0x13` | Gained flag (u8) |

### String Encoding

Strings are length-prefixed UTF-8:
```
┌────────────┬─────────────────┐
│ length     │     bytes       │
│  (u16 LE)  │    (UTF-8)      │
└────────────┴─────────────────┘
```

---

## Capability Types

When declaring capabilities in the manifest:

| ObjectType | Value | Description |
|------------|-------|-------------|
| `Endpoint` | 1 | IPC endpoint for communication |
| `Console` | 2 | Console/debug output |
| `Storage` | 3 | Persistent storage (per-app namespace) |
| `Network` | 4 | Network access |
| `Process` | 5 | Process management |
| `Memory` | 6 | Memory region |

---

## Build & Test

After creating all files:

```powershell
# Build the WASM binary
.\build.ps1 processes

# Or with make
make build-processes

# Start dev server
cd web && npm run dev
```

---

## Checklist

- [ ] Created `crates/zos-apps/src/bin/{app_name}.rs`
- [ ] Added manifest to `crates/zos-apps/src/manifest.rs`
- [ ] Created `crates/zos-apps/src/app_protocol/{app_name}.rs`
- [ ] Updated `crates/zos-apps/src/app_protocol/mod.rs` (mod + pub use + type tag)
- [ ] Added `[[bin]]` to `crates/zos-apps/Cargo.toml`
- [ ] Created `web/apps/{AppName}App/{AppName}App.tsx` (using ZUI components)
- [ ] Created `web/apps/{AppName}App/{AppName}App.module.css`
- [ ] Added decoder to `web/apps/shared/app-protocol.ts`
- [ ] Added route to `web/apps/AppRouter/AppRouter.tsx`
- [ ] Updated `build.ps1` with copy command
- [ ] Updated `Makefile` with copy command
- [ ] Verified using ZUI components (Panel, Button, Text, Label) - NO raw HTML
