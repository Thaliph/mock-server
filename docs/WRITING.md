# Spec style: Simplified Technical English

The spec (`docs/SPEC.md`), the design notes and the step specs are written for developers, reviewers
and operators whose first language is not always English. We write them in the style of
**ASD-STE100 Simplified Technical English** (STE). STE is a controlled language from the aerospace
industry: one word has one meaning, sentences are short, and each instruction is one action. The rules
below are our short form of the STE writing rules. They apply to all new or changed text in these
documents:

- the spec;
- the design notes;
- the issue comments of the architect and the spec writer;
- the wiki.

## Words

1. Use one term for one thing. Do not use synonyms. When the spec names a thing (`view`, `contract`,
   `try`, `refusal`), use that name every time. Do not change to "attempt", "admission rule" or
   "rejection".
2. Use a word with its main meaning only. "Close" is "to close a connection", not "near".
3. Use simple verbs: *use*, *send*, *refuse*, *count*, *keep*. Do not use phrasal verbs ("set up",
   "carry out", "get rid of") when a simple verb exists ("configure", "do", "remove").
4. Do not use the *-ing* form as a noun or after a preposition ("after reading the head"). Write "after
   the sidecar reads the head".
5. Code names, field names, metric names and error values are technical names. Write them in
   backticks, exactly as the code writes them.
6. Do not write more than three nouns in a row. "The admission contract queue deadline" becomes "the
   deadline of the queue of the admission contract".

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
15. An acceptance criterion starts with its test level (§12.2, or an extra level that its section
    defines). Then it gives the setup, the action and the expected result, in that order. Each
    expected result is a separate sentence or list item that a test can check.
16. Put a warning or a security limit before the instruction that it applies to.

## Example

Before:

> Requests that find no free slot of the contract are held by the inbound sidecar in a bounded queue,
> being refused when it is full or after its 150 ms deadline has been reached.

After:

> If the contract has no free slot, the inbound sidecar puts the request in the queue. If the queue
> is full, the sidecar refuses the request with `overloaded`. A request must not stay in the queue
> for more than 150 ms. After 150 ms, the sidecar refuses the request with `queue-timeout`.

## Scope

New and changed text follows these rules now. Old text is changed to STE when a step changes it
anyway. A rewrite of a whole section is a separate pull request with no change of behaviour.
