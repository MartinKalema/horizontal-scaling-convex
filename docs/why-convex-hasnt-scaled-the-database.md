# Why Convex Hasn't Scaled the Database

It's not a technical problem. It's a business decision.

## The Business Logic

Convex Cloud is their product. Self-hosted is free. If self-hosted scales horizontally, the main reason to pay for Cloud disappears. Their pricing tiers (S16, S256, D1024) are vertical scaling — you pay more for a bigger machine. Horizontal scaling would let you add cheap nodes instead of upgrading to an expensive tier.

Their engineers absolutely could build this. James Cowling built distributed storage at Dropbox (Magic Pocket — exabytes of data). Their team includes people from Google and Facebook infrastructure. They know how to build distributed systems.

They chose not to — for the same reason MongoDB kept sharding as an enterprise feature, Redis kept clustering as a paid add-on, and Elasticsearch charges for cross-cluster replication. The hard scaling problems are the moat that justifies the managed service.

## The Evidence

The Funrun scaling they did build supports this theory. Funrun scales function execution — which hits limits visibly (128 V8 threads, customer complaints about concurrency). Scaling compute makes customers happy without removing the need for Cloud. The database remaining single-node means that as your app grows, you eventually need their bigger deployment classes, which means more revenue.

## The Pattern

This is rational business strategy. Build the product people love, keep the scaling hard enough that self-hosting at scale isn't practical, and offer Cloud as the solution. Every infrastructure company does this.

## Why We Could

We didn't have that constraint. We're not selling a managed service. We had a codebase, a problem, and no business reason not to solve it.
