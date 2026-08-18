# Blue-Collar Engineering Style Guide

## Voice & Tone

The newsletter voice matches "DevOps for the Desperate": hands-on, no-nonsense, practical. Write like you are explaining something to a colleague, not lecturing.

### Personality Traits
- Conversational, not formal
- Self-aware about the dramatized stories
- Technically credible
- Fair and balanced
- Encouraging

### Sound Human, Not AI

AI writing tends to be overly structured, hedging, and formal. Avoid these patterns:

**AI patterns to avoid:**
- Excessive qualifiers: "It's important to note that..."
- Meta-commentary: "Let me explain..." or "In this section, we'll cover..."
- Hedging phrases: "It could be argued that..." or "One might consider..."
- Formal transitions: "Furthermore," "Additionally," "Moreover,"
- Summarizing what you just said

**Write like this instead:**
- Get to the point
- Use short sentences mixed with longer ones
- Let ideas flow naturally without announcing them
- Trust the reader to follow along

## Punctuation Rules

### No Double Hyphens
Never use `--` in the text. Use proper punctuation instead:

**Wrong:** `Kubernetes is powerful -- but most teams don't need it`
**Right:** `Kubernetes is powerful, but most teams don't need it`
**Also right:** `Kubernetes is powerful. Most teams don't need it.`

If you need an em dash for an aside, restructure the sentence or use commas/parentheses.

### Quotes Are for Dialogue Only

Save quotes for the opening story where characters speak. Outside the story section, express thoughts and ideas directly without quotation marks.

**In the story (quotes OK):**
```
"Why is this so hard?" Rishi yelled. "We used to deploy in five minutes!"
```

**Outside the story (no quotes):**
```
Wrong: Over engineering leaves us thinking, "Why is this so complicated?"
Right: Over engineering leaves us wondering why everything got so complicated.
```

**Exception:** Direct quotes from external sources with citations are fine.

## Writing Techniques

### Rhetorical Questions
Use sparingly to engage readers:
- Why do so many engineers design systems as if they're preparing for an alien invasion?
- But here's the thing: most of us aren't launching rockets or running the next Netflix.

### Specific Details Over Vague Claims
Instead of: Kubernetes is complex

Write: You'll need to configure Helm, manage YAML files for every deployment, set up ingress controllers, and troubleshoot pod failures at 2 AM.

### Data and Sources
Back claims with specifics. Include the source:
- According to a D2iQ study, 47% of enterprises cited security risks as their top Kubernetes challenge.
- Shopify handles ~60M requests per minute on Rails.

## Formatting

### Headers
- Use `###` for main sections
- Use `####` for subsections
- Keep headers short

### Code Blocks
- Always include language identifier
- Add comments for non-obvious steps
- Make code runnable

### Lists
Bullet points for problems, requirements, options.
Numbered lists for sequential steps.

### Emphasis
- **Bold** for key concepts
- *Italics* sparingly for emphasis
- Never ALL CAPS

## Common Patterns

### Transition After Story
- First off, thanks for indulging the story and not letting the eye rolls stop you from finishing.
- I know, the story is contrite and silly, but the pattern is real.
- Enough with the lead in. Let's talk about [topic].

### Acknowledging Complexity Has Its Place
- [Tool] becomes valuable as systems and teams grow.
- That said, [tool] might be right when...
- Let your needs drive the decision, not tech media.

### Closing
```
Until next time,
Bradley
*Chief Advocate for Keeping It Simple*
```

## Words to Use
- blue-collar engineering
- hands-on
- no-nonsense
- practical
- boring, proven technology
- cognitive load
- premature optimization
- modular monolith

## Words to Avoid
- best practices (corporate speak)
- scalable solution (vague)
- cutting-edge (implies complexity is good)
- enterprise-grade (usually means over-engineered)
- you should never (too prescriptive)
- obviously, simply, just (condescending)
- it's important to note (AI filler)
- let's dive in (overused)

## Audience

Write for mid-level to senior engineers at startups or small teams who have felt the pain of over-engineering. They are technical enough for code examples but time-constrained.

Do not assume they agree with you. Acknowledge tradeoffs honestly.
