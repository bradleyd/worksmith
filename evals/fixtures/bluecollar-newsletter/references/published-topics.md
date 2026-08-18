# Published Newsletters

Track of published editions to avoid topic repetition.

## Published

### #0: "You Don't Need Kubernetes (Yet)" - January 2025
**Story:** Brewly, a coffee delivery startup, over-engineers with EKS cluster
**Core argument:** K8s requires expertise most teams don't have; simpler hosting works for most
**Hands-on:** Deploying to EC2 with Docker (no K8s)
**Key stats:** D2iQ study on K8s adoption challenges (Security 47%, Scalability 37%)

### #1: "Microservices: Double-Edged Sword" - February 2025
**Story:** (Derived from Feature Frenzy narrative)
**Core argument:** Microservices add complexity; start with modular monolith
**Key points:** API contract overhead, synchronized deployments, fit for purpose
**Takeaway:** Transition to microservices only when the time comes

### #2: "The Overmonitoring Trap" - May 2025
**Story:** (TBD - likely MetricMania or similar)
**Core argument:** Simple metrics (golden signals) beat complex Prometheus setups
**Focus:** When simple metrics are enough

### #3: "SQLite: The Database You're Not Using (But Should Be)" - September 2025
**Story:** (TBD)
**Core argument:** SQLite handles more than people think; PostgreSQL often overkill
**Key points:** Zero-config, automatic backups, modern use cases (RAG, LLM caching)
**Key stats:** Concrete metrics on SQLite scalability

## Planned/Future Topics

These have been identified as future candidates:

- **Logging**: Structured logging implementations gone wrong
- **CI/CD**: When Jenkins/complex pipelines are overkill
- **Kafka**: Message queues you probably don't need
- **Caching**: Redis overuse for simple use cases
- **Static typing**: When dynamic languages are perfectly fine
- **Database sharding**: Premature optimization trap

## Topic Selection Criteria

A good Blue-Collar newsletter topic:
1. Is commonly over-engineered in the industry
2. Has a genuinely simpler alternative
3. Can be illustrated with a relatable startup story
4. Has concrete data/metrics to support the argument
5. Includes a hands-on tutorial for the simpler approach
6. Acknowledges when the complex solution IS appropriate
