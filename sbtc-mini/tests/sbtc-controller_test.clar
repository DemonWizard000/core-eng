(define-constant test-contract-principal (as-contract tx-sender))
(define-constant zero-address 'SP000000000000000000002Q6VF78)

;; @name Protocol contracts can trigger upgrade
;; @as-protocol
(define-public (test-upgrade)
	(contract-call? .sbtc-controller upgrade (list {contract: test-contract-principal, enabled: true}))
)

;; @name Upgrade principals must be contract principals
;; @as-protocol
(define-public (test-upgrade-principals)
	(let ((result (contract-call? .sbtc-controller upgrade (list {contract: zero-address, enabled: true}))))
		(asserts! (is-eq (element-at? (unwrap-panic result) u0) (some false)) (err "Should have been false"))
		(ok true)
	)
)

;; @name Non-protocol principals cannot trigger upgrade
(define-public (test-upgrade-non-prototcol-principal)
	(match (contract-call? .sbtc-controller upgrade (list {contract: test-contract-principal, enabled: true}))
		success (err "Should have failed")
		error (ok true)
	)
)
