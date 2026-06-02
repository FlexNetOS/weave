```markdown
# weave Development Patterns

> Auto-generated skill from repository analysis

## Overview
This skill teaches you the core development patterns and conventions used in the `weave` Rust codebase. You'll learn about file naming, import/export styles, commit message habits, and how to structure and run tests. This guide is designed to help contributors maintain consistency and quality in the project.

## Coding Conventions

### File Naming
- Use **camelCase** for file names.
  - Example: `dataProcessor.rs`, `userSession.rs`

### Import Style
- Use **relative imports** within the codebase.
  - Example:
    ```rust
    use crate::utils::stringHelpers;
    use super::config;
    ```

### Export Style
- Use **named exports** for modules and functions.
  - Example:
    ```rust
    pub fn process_data() { ... }
    pub struct UserSession { ... }
    ```

### Commit Messages
- Freeform style, often prefixed with `weave`.
- Average commit message length: ~150 characters.
  - Example: `weave: refactor dataProcessor to improve error handling and add logging for debug mode`

## Workflows

### Adding a New Module
**Trigger:** When you need to introduce a new feature or component.
**Command:** `/add-module`

1. Create a new file using camelCase naming (e.g., `featureHandler.rs`).
2. Implement your module using relative imports for dependencies.
3. Export public functions or structs with `pub`.
4. Write associated tests in a corresponding `*.test.*` file.
5. Commit changes with a descriptive message, prefixed with `weave` if appropriate.

### Refactoring Existing Code
**Trigger:** When improving or restructuring code for clarity or performance.
**Command:** `/refactor-code`

1. Identify the target files and functions.
2. Use relative imports for any new dependencies.
3. Maintain camelCase naming for new files.
4. Update exports to remain named and public as needed.
5. Update or add tests to cover refactored logic.
6. Commit with a detailed message, e.g., `weave: refactor session management for async support`.

### Writing and Running Tests
**Trigger:** When adding new features or fixing bugs.
**Command:** `/run-tests`

1. Create or update test files following the `*.test.*` pattern (e.g., `userSession.test.rs`).
2. Write tests using the project's preferred Rust testing approach.
3. Run tests using the standard Rust test runner:
    ```sh
    cargo test
    ```
4. Ensure all tests pass before committing.

## Testing Patterns

- Test files use the `*.test.*` naming pattern (e.g., `module.test.rs`).
- The specific testing framework is not detected, but Rust's built-in test framework is likely used.
- Example test structure:
    ```rust
    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn test_feature() {
            assert_eq!(feature_function(), expected_value);
        }
    }
    ```

## Commands
| Command         | Purpose                                         |
|-----------------|-------------------------------------------------|
| /add-module     | Scaffold and implement a new module             |
| /refactor-code  | Refactor existing code with proper conventions  |
| /run-tests      | Run all tests in the codebase                   |
```
