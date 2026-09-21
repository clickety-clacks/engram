# Engram - Agent Instructions

> The role of this file is to describe common mistakes and confusion points that agents might encounter as they work in this project. If you ever encounter something in the project that surprises you, please alert the developer working with you and indicate that this is the case in the AgentMD file to help prevent future agents from having the same issue.

> This is a greenfield app with no users. Feel free to suggest structural and breaking refactors to help bend this codebase into the right shape.


Native append lineage: dispatch rows retain first occurrences only within each tape. Query selection must fold immutable predecessor segments before choosing a UUID/direction; a repeated marker must not renew its turn. Recovered hops retain context in `session` and the original evidence child in `edit_session`; public chain formatting must use the latter when present.
