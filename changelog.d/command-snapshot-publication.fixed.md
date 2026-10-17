Publish acknowledged command snapshots once, without repeating the same update
at the end of an idle command batch or during subsequent read-only queries.
Real streaming progress and storage faults still publish updates, and command
snapshots remain visible before acknowledgement.
