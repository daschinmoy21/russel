// env-config shows non-secret env and whether a secret ref was injected.
// Never prints secret values — only presence and length.
package main

import (
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"log"
	"net/http"
	"os"
	"os/signal"
	"strconv"
	"syscall"
	"time"
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
			"greeting":   os.Getenv("GREETING"),
			"log_level":  os.Getenv("LOG_LEVEL"),
			"secret_set": secret != "",
			"secret_len": len(secret),
			"note":       "secret value is never returned by this endpoint",
		}
		w.Header().Set("Content-Type", "application/json")
		_ = json.NewEncoder(w).Encode(out)
	})

	server := &http.Server{
		Addr:              fmt.Sprintf(":%d", port),
		Handler:           mux,
		ReadHeaderTimeout: 5 * time.Second,
	}

	errs := make(chan error, 1)
	go func() {
		log.Printf("env-config listening on %s", server.Addr)
		errs <- server.ListenAndServe()
	}()

	signals := make(chan os.Signal, 1)
	signal.Notify(signals, os.Interrupt, syscall.SIGTERM)

	select {
	case sig := <-signals:
		log.Printf("received %s, shutting down", sig)
	case err := <-errs:
		if !errors.Is(err, http.ErrServerClosed) {
			log.Fatal(err)
		}
		return
	}

	ctx, cancel := context.WithTimeout(context.Background(), 5*time.Second)
	defer cancel()
	if err := server.Shutdown(ctx); err != nil {
		log.Fatal(err)
	}
}
