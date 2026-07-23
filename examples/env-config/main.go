// env-config shows non-secret env and whether a secret ref was injected.
// Never prints secret values — only presence and length.
package main

import (
	"encoding/json"
	"fmt"
	"log"
	"net/http"
	"os"
	"strconv"
)

func main() {
	port := 3000
	if v := os.Getenv("PORT"); v != "" {
		if p, err := strconv.Atoi(v); err == nil && p > 0 && p < 65536 {
			port = p
		}
	}

	mux := http.NewServeMux()
	mux.HandleFunc("/health", func(w http.ResponseWriter, _ *http.Request) {
		w.Header().Set("Content-Type", "text/plain")
		fmt.Fprintln(w, "ok")
	})
	mux.HandleFunc("/", func(w http.ResponseWriter, _ *http.Request) {
		secret := os.Getenv("DEMO_SECRET")
		out := map[string]any{
			"greeting":       os.Getenv("GREETING"),
			"log_level":      os.Getenv("LOG_LEVEL"),
			"secret_set":     secret != "",
			"secret_len":     len(secret),
			"note":           "secret value is never returned by this endpoint",
		}
		w.Header().Set("Content-Type", "application/json")
		_ = json.NewEncoder(w).Encode(out)
	})

	addr := fmt.Sprintf(":%d", port)
	log.Printf("env-config listening on %s", addr)
	log.Fatal(http.ListenAndServe(addr, mux))
}
