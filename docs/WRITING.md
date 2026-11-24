# Writing the spec: Simplified Technical English

The spec (`docs/SPEC.md`), the design notes and the step specs are written for developers, reviewers
and operators whose first language is not always English. We write them in the style of
**ASD-STE100 Simplified Technical English** (STE). STE is a controlled language from the aerospace
industry: one word has one meaning, sentences are short, and each instruction is one action. The rules
below are our short form of the STE writing rules. They apply to all new or changed text in the spec,
the design notes, the issue comments of the architect and the spec writer, and the wiki.

## Words

1. Use one term for one thing. Do not use synonyms. When the spec names a thing (`view`, `contract`,
   `try`, `refusal`), use that name every time. Do not change to "attempt", "limit" or "rejection".
2. Use a word with its main meaning only. "Close" is "to close a connection", not "near".
3. Use simple verbs: *use*, *send*, *refuse*, *count*, *keep*. Do not use phrasal verbs ("set up",
   "carry out", "get rid of") when a simple verb exists ("configure", "do", "remove").
4. Do not use the *-ing* form as a noun or after a preposition ("after reading the head"). Write "after
   the sidecar reads the head".
5. Code names, field names, metric names and error values are technical names. Write them in
   backticks, exactly as the code writes them.
6. Do not write more than three nouns in a row. "The admission contract queue deadline" becomes "the
   deadline of the admission queue".

## Sentences

7. One sentence gives one statement or one instruction.
8. A sentence that tells someone what to do has at most **20 words**. A sentence that describes has at
   most **25 words**.
9. Use the active voice: "The sidecar refuses the request", not "the request is refused". Use the
   passive only when the actor is not known or not important.
10. Use the present tense for behaviour: "The controller writes the status." Use "must" for a
    requirement and "can" for a permitted action. Do not use "should", "may" or "will" in a criterion.
11. Use the article ("the", "a") where English permits it. Do not delete words to make a sentence
    shorter.
12. Give the condition first, then the result: "If the view is stale, the sidecar keeps the last
    contract."

## Paragraphs, lists and criteria

13. One paragraph has one topic and at most six sentences.
14. Use a vertical list for three or more items, steps or conditions. Number the steps when their order
    is important.
15. An acceptance criterion starts with its test level (**U**, **I**, **E**, **B**, **D**), then gives
    the setup, the action and the expected result, in that order. Each expected result is a separate
    sentence or list item that a test can check.
16. Put a warning or a security limit before the instruction that it applies to.

## Example

Before:

> Requests over the contract are refused by the inbound sidecar after having been held in a bounded
> queue whose deadline, which is 150 ms, is enforced per request, unless the head can't be judged.

After:

> The inbound sidecar refuses a request that is over the contract. Before it refuses the request, it
> keeps the request in a bounded queue. Each request can wait in the queue for 150 ms at most. This
> rule does not apply to a head that the sidecar cannot read (§4.1).

## Scope

New and changed text follows these rules now. Old text is changed to STE when a step changes it
anyway. A rewrite of a whole section is a separate pull request with no change of behaviour.
