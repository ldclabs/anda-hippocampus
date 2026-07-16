package api

import (
	"context"
	"io"
	"net/http"
	"net/http/httptest"
	"testing"
)

func TestClientHeaders(t *testing.T) {
	var gotShard, gotAuth, gotAccept string
	server := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		gotShard = r.Header.Get("Shard-Id")
		gotAuth = r.Header.Get("Authorization")
		gotAccept = r.Header.Get("Accept")
		w.Header().Set("Content-Type", "application/json")
		_, _ = w.Write([]byte(`{"name":"brain","version":"0.6.12","sharding":3,"description":"d"}`))
	}))
	defer server.Close()

	client := NewClient(server.URL, "s1", "tok")
	client.Shard = 3
	info, err := client.GetInfo(context.Background())
	if err != nil {
		t.Fatalf("GetInfo returned error: %v", err)
	}
	if info.Sharding != 3 {
		t.Fatalf("unexpected info: %+v", info)
	}
	if gotShard != "3" {
		t.Fatalf("expected Shard-Id header 3, got %q", gotShard)
	}
	if gotAuth != "Bearer tok" {
		t.Fatalf("unexpected Authorization header: %q", gotAuth)
	}
	if gotAccept != "application/json" {
		t.Fatalf("unexpected Accept header: %q", gotAccept)
	}
}

func TestClientNoShardHeaderByDefault(t *testing.T) {
	var shardPresent bool
	server := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		_, shardPresent = r.Header["Shard-Id"]
		w.Header().Set("Content-Type", "application/json")
		_, _ = w.Write([]byte(`{"name":"brain","version":"0.6.12","sharding":0,"description":"d"}`))
	}))
	defer server.Close()

	client := NewClient(server.URL, "s1", "")
	if _, err := client.GetInfo(context.Background()); err != nil {
		t.Fatalf("GetInfo returned error: %v", err)
	}
	if shardPresent {
		t.Fatalf("Shard-Id header should not be sent when shard is 0")
	}
}

func TestRevokeSpaceTokenSendsToken(t *testing.T) {
	var gotPath, gotBody string
	server := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		gotPath = r.URL.Path
		body, _ := io.ReadAll(r.Body)
		gotBody = string(body)
		w.Header().Set("Content-Type", "application/json")
		_, _ = w.Write([]byte(`{"result":true}`))
	}))
	defer server.Close()

	client := NewClient(server.URL, "s1", "tok")
	resp, err := client.RevokeSpaceToken(context.Background(), "STabc")
	if err != nil {
		t.Fatalf("RevokeSpaceToken returned error: %v", err)
	}
	if resp.Error != nil || resp.Result == nil || !*resp.Result {
		t.Fatalf("unexpected response: %+v", resp)
	}
	if gotPath != "/v1/s1/management/revoke_space_token" {
		t.Fatalf("unexpected path: %q", gotPath)
	}
	if gotBody != `{"token":"STabc"}` {
		t.Fatalf("unexpected body: %s", gotBody)
	}
}

func TestRevokeSpaceTokenByNameSendsName(t *testing.T) {
	var gotPath, gotBody string
	server := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		gotPath = r.URL.Path
		body, _ := io.ReadAll(r.Body)
		gotBody = string(body)
		w.Header().Set("Content-Type", "application/json")
		_, _ = w.Write([]byte(`{"result":true}`))
	}))
	defer server.Close()

	client := NewClient(server.URL, "s1", "tok")
	resp, err := client.RevokeSpaceTokenByName(context.Background(), "reader")
	if err != nil {
		t.Fatalf("RevokeSpaceTokenByName returned error: %v", err)
	}
	if resp.Error != nil || resp.Result == nil || !*resp.Result {
		t.Fatalf("unexpected response: %+v", resp)
	}
	if gotPath != "/v1/s1/management/revoke_space_token" {
		t.Fatalf("unexpected path: %q", gotPath)
	}
	if gotBody != `{"name":"reader"}` {
		t.Fatalf("unexpected body: %s", gotBody)
	}
}

func TestClientErrorEnvelope(t *testing.T) {
	server := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		w.Header().Set("Content-Type", "application/json")
		w.WriteHeader(http.StatusBadRequest)
		_, _ = w.Write([]byte(`{"message":"space not found"}`))
	}))
	defer server.Close()

	client := NewClient(server.URL, "missing", "")
	_, err := client.GetSpaceInfo(context.Background())
	if err == nil {
		t.Fatalf("expected error for HTTP 400")
	}
	want := "HTTP 400: space not found"
	if err.Error() != want {
		t.Fatalf("unexpected error: %q, want %q", err.Error(), want)
	}
}
