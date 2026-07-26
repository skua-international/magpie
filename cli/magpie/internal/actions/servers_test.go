package actions

import "testing"

func TestSanitizeServerName(t *testing.T) {
	cases := []struct {
		name    string
		in      string
		want    string
		wantErr bool
	}{
		{name: "already valid", in: "ops", want: "ops"},
		{name: "uppercase lowercased", in: "Ops", want: "ops"},
		{name: "mixed case with digits and dash", in: "Ops-2", want: "ops-2"},
		{name: "empty", in: "", wantErr: true},
		{name: "space not allowed even after lowercasing", in: "test server", wantErr: true},
		{name: "leading dash", in: "-ops", wantErr: true},
		{name: "trailing dash", in: "ops-", wantErr: true},
		{name: "too long", in: func() string {
			s := make([]byte, 64)
			for i := range s {
				s[i] = 'a'
			}
			return string(s)
		}(), wantErr: true},
	}
	for _, tc := range cases {
		t.Run(tc.name, func(t *testing.T) {
			got, err := SanitizeServerName(tc.in)
			if tc.wantErr {
				if err == nil {
					t.Fatalf("SanitizeServerName(%q) = %q, nil; want error", tc.in, got)
				}
				return
			}
			if err != nil {
				t.Fatalf("SanitizeServerName(%q) unexpected error: %v", tc.in, err)
			}
			if got != tc.want {
				t.Fatalf("SanitizeServerName(%q) = %q; want %q", tc.in, got, tc.want)
			}
		})
	}
}
