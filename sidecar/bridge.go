package main

import (
	"encoding/json"
	"fmt"
	"io"
	"log"
	"net/http"
	"os"
	"strings"
	"time"
)

type ProxyRequest struct {
	Method  string            `json:"method"`
	URL     string            `json:"url"`
	Headers map[string]string `json:"headers,omitempty"`
	Body    json.RawMessage   `json:"body,omitempty"`
}

type ProxyResponse struct {
	Status  int               `json:"status"`
	Headers map[string]string `json:"headers"`
	Body    json.RawMessage   `json:"body"`
}

func main() {
	addr := os.Getenv("BRIDGE_ADDR")
	if addr == "" {
		addr = ":8080"
	}

	client := &http.Client{
		Timeout: 60 * time.Second,
	}

	mux := http.NewServeMux()

	mux.HandleFunc("/health", func(w http.ResponseWriter, r *http.Request) {
		w.Header().Set("Content-Type", "application/json")
		w.Write([]byte(`{"status":"ok"}`))
	})

	mux.HandleFunc("/proxy", func(w http.ResponseWriter, r *http.Request) {
		if r.Method != http.MethodPost {
			http.Error(w, "method not allowed", http.StatusMethodNotAllowed)
			return
		}

		var req ProxyRequest
		if err := json.NewDecoder(r.Body).Decode(&req); err != nil {
			http.Error(w, fmt.Sprintf("invalid request: %v", err), http.StatusBadRequest)
			return
		}

		var bodyReader io.Reader
		if req.Body != nil {
			bodyReader = strings.NewReader(string(req.Body))
		}

		httpReq, err := http.NewRequest(req.Method, req.URL, bodyReader)
		if err != nil {
			http.Error(w, fmt.Sprintf("failed to create request: %v", err), http.StatusBadRequest)
			return
		}

		for k, v := range req.Headers {
			httpReq.Header.Set(k, v)
		}

		log.Printf("proxying %s %s", req.Method, req.URL)

		resp, err := client.Do(httpReq)
		if err != nil {
			log.Printf("proxy error: %v", err)
			http.Error(w, fmt.Sprintf("proxy error: %v", err), http.StatusBadGateway)
			return
		}
		defer resp.Body.Close()

		respBody, err := io.ReadAll(resp.Body)
		if err != nil {
			http.Error(w, fmt.Sprintf("failed to read response: %v", err), http.StatusBadGateway)
			return
		}

		respHeaders := make(map[string]string)
		for k := range resp.Header {
			respHeaders[k] = resp.Header.Get(k)
		}

		proxyResp := ProxyResponse{
			Status:  resp.StatusCode,
			Headers: respHeaders,
			Body:    json.RawMessage(respBody),
		}

		w.Header().Set("Content-Type", "application/json")
		json.NewEncoder(w).Encode(proxyResp)

		log.Printf("proxied %s %s -> %d", req.Method, req.URL, resp.StatusCode)
	})

	log.Printf("bridge server listening on %s", addr)
	if err := http.ListenAndServe(addr, mux); err != nil {
		log.Fatalf("server error: %v", err)
	}
}
