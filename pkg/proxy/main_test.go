package proxy

import (
	"os"
	"testing"

	"github.com/rs/zerolog"
	"github.com/rs/zerolog/log"
)

func TestMain(m *testing.M) {
	if os.Getenv("PROXY_TEST_LOGS") == "" {
		log.Logger = zerolog.Nop()
	}
	os.Exit(m.Run())
}
