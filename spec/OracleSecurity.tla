--------------------------- MODULE OracleSecurity ---------------------------
EXTENDS Naturals, Sequences, FiniteSets, TLC

CONSTANTS Seats, Rooms, PrivateRooms, MaxRecords,
          BreakAppend, DropWithheld, LeakScope, LeakPrivate
VARIABLES log, priorLog, scopes, members, lastRead
vars == <<log, priorLog, scopes, members, lastRead>>

Allowed(s, r) == r \in scopes[s] /\ (r \notin PrivateRooms \/ s \in members)
Visible(s, r) == (r \in scopes[s] \/ LeakScope)
                 /\ (r \notin PrivateRooms \/ s \in members \/ LeakPrivate)

Init == /\ log = <<>> /\ priorLog = <<>>
        /\ scopes = [s \in Seats |-> Rooms]
        /\ members = Seats
        /\ lastRead = [visible |-> FALSE, scoped |-> FALSE,
                        member |-> FALSE, private |-> FALSE]

AppendRecord(r) ==
  /\ Len(log) < MaxRecords
  /\ priorLog' = log
  /\ log' = Append(IF BreakAppend /\ Len(log) > 0 THEN Tail(log) ELSE log,
                    [id |-> Len(log) + 1, prev |-> Len(log), room |-> r])
  /\ UNCHANGED <<scopes, members, lastRead>>

ChangeScope(s, r) ==
  /\ scopes' = [scopes EXCEPT ![s] = IF r \in @ THEN @ \ {r} ELSE @ \cup {r}]
  /\ priorLog' = log
  /\ UNCHANGED <<log, members, lastRead>>

ChangeMember(s) ==
  /\ members' = IF s \in members THEN members \ {s} ELSE members \cup {s}
  /\ priorLog' = log
  /\ UNCHANGED <<log, scopes, lastRead>>

Read(s, r) ==
  /\ lastRead' = [visible |-> Visible(s, r), scoped |-> r \in scopes[s],
                   member |-> s \in members, private |-> r \in PrivateRooms]
  /\ priorLog' = log
  /\ UNCHANGED <<log, scopes, members>>

Next == (\E r \in Rooms : AppendRecord(r))
        \/ (\E s \in Seats, r \in Rooms : ChangeScope(s, r) \/ Read(s, r))
        \/ (\E s \in Seats : ChangeMember(s))

Projected(s) ==
  LET retained == IF DropWithheld
                  THEN SelectSeq(log, LAMBDA e : Allowed(s, e.room)) ELSE log
  IN [i \in 1..Len(retained) |->
       [prev |-> retained[i].prev,
        hidden |-> ~Allowed(s, retained[i].room),
        payload |-> IF Allowed(s, retained[i].room) THEN "content" ELSE ""]]

TypeOK == /\ log \in Seq([id : 1..MaxRecords, prev : 0..MaxRecords, room : Rooms])
          /\ Len(log) <= MaxRecords
          /\ scopes \in [Seats -> SUBSET Rooms]
          /\ members \subseteq Seats
AppendOnly == /\ Len(log) >= Len(priorLog)
              /\ SubSeq(log, 1, Len(priorLog)) = priorLog
ChainWhole == \A i \in 1..Len(log) : log[i].id = i /\ log[i].prev = i - 1
WithheldWhole == \A s \in Seats :
  /\ Len(Projected(s)) = Len(log)
  /\ \A i \in 1..Len(Projected(s)) :
       /\ Projected(s)[i].prev = i - 1
       /\ (Projected(s)[i].hidden => Projected(s)[i].payload = "")
ScopeSafety == lastRead.visible => lastRead.scoped
PrivateSafety == lastRead.visible /\ lastRead.private => lastRead.member

Spec == Init /\ [][Next]_vars
=============================================================================
